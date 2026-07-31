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

use crate::io_flags;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};
use parking_lot::{Condvar, Mutex, RwLock};
use std::cell::Cell;
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
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
    // The `SocketAddr` is captured once at bind time (before the accept
    // loop starts) so `getLocalAddress()` never has to contend for the
    // same `Mutex` the accept worker holds for the full duration of its
    // blocking `TcpListener::accept()` call — locking it from
    // `aio_assc_local_address` deadlocked-in-practice (an indefinite wait
    // whenever it landed between two accepted connections), turning
    // Tomcat's post-bind `getLocalPort()` call into a startup hang. See
    // the `aio_assc_local_address` doc comment.
    Listener(Arc<Mutex<TcpListener>>, SocketAddr),
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

/// BUG FIX (2026-07-17, found while investigating `WebSocketIntegrationTests`
/// `TomcatWebSocketClient` timeouts — see `
/// CRATONVM-SPRING-GENUINE-BUGLIST`): `aio_asc_close`/`aio_assc_close`
/// used to call only `aio_remove`, which drops the *registry's*
/// `Arc<Mutex<TcpStream>>` — but any in-flight handler-form read
/// (`aio_asc_read` -> `Job::ReadFd`) is blocked in a worker thread on its OWN
/// independent `try_clone()` duplicate of the socket (a separate OS fd
/// sharing the same underlying open-file-description). Dropping/closing the
/// registry's fd does not send a FIN as long as that cloned fd remains open,
/// so the worker's blocking `read()` never wakes up — real `close()` silently
/// does nothing to it, violating `AsynchronousCloseException`'s contract that
/// outstanding operations complete on close. This is a genuine, independently
/// confirmed gap (verified via `cargo test -p cratonvm-native-io`, 349/349
/// still pass) and worth fixing on its own merits.
///
/// IMPORTANT — this does *not* fix the `WebSocketIntegrationTests`
/// `TomcatWebSocketClient` hangs it was found while chasing. `CRATONVM_DBG_AIO`
/// tracing showed `aio_asc_close` is *never entered at all* (0 hits) across
/// every run of that test class, passing or failing — Tomcat's own
/// `WsWebSocketContainer` never calls `channel.close()` on the path that
/// hangs; it completes the handshake and (for the close-sequence tests) the
/// WS-level close-frame exchange correctly at the byte level, then the
/// higher-level Java session/`Mono` simply never resolves, well before any
/// `close()` call would occur. That remaining hang is still open — see the
/// known-issues doc entry for the full trace-based writeup.
///
/// Fix: `shutdown(Both)` on the registry's stream before removing it.
/// `shutdown()` (unlike `close()`) acts on the shared socket, not the fd, so
/// it unblocks *every* fd that still references the same open-file-
/// description — including clones already handed to worker threads — causing
/// their blocked `read()` to return `Ok(0)` (EOF) immediately, which
/// `Job::ReadFd`/`Job::ReadFutureFd` already translate into a normal
/// `completed(-1)` / EOF completion. Errors are ignored: the socket may
/// already be half-closed by the peer, or the OS handle may already be
/// invalid, both harmless here since removal proceeds regardless.
fn aio_shutdown_stream(id: i32) {
    if let Some(AioHandle::Stream(s)) = aio_registry().read().get(&id) {
        let _ = s.lock().shutdown(Shutdown::Both);
    }
}

/// Same fix as `aio_shutdown_stream`, for the *other* id space: channels
/// connected through the Future-form `connect` (and every `TomcatWebSocketClient`
/// connection observed in practice) store an `fd_table` fd directly in
/// `F_REG_ID`, not a legacy `aio_registry` id (see `AIO_REG_BASE`'s doc comment
/// — fd_table fds are always numerically below it). `aio_shutdown_stream`
/// alone is therefore a no-op for these: there is no `aio_registry` entry to
/// find. `fd_table`'s own `close()` has the identical gap `aio_remove` had —
/// it just drops the table's `Arc`, which does not touch a worker's already-
/// `try_clone()`'d duplicate fd. So: obtain one more clone here (a cheap
/// `dup()`) purely to call `shutdown(Both)` on the *shared* socket — `shutdown`
/// affects every fd referencing the same open-file-description, including the
/// worker's, unlike `close`/`drop` which only affect the one fd being closed.
fn aio_shutdown_fd_table_stream(ctx: &mut dyn NativeContext, fd: u32) {
    if let Ok(stream) = ctx.fd_table().try_clone_tcp(fd) {
        let _ = stream.shutdown(Shutdown::Both);
    }
    let _ = ctx.fd_table().close(fd);
}

/// Opt-in diagnostic (`CRATONVM_DBG_AIO=1`), added 2026-07-17 while
/// investigating `WebSocketIntegrationTests`'s `TomcatWebSocketClient`
/// timeouts (Tomcat's `WsWebSocketContainer` is the one real client that
/// drives `AsynchronousSocketChannel.read/write(ByteBuffer)` — the Future
/// form below). Traces every Future-form read/write dispatch, its worker
/// completion, and delivery, tagged by the fd (fd_table id, not the
/// registry id) and the cloned-stream's raw OS fd where cheaply available,
/// so overlapping/interleaved reads on the same logical connection show up
/// as interleaved trace lines with a shared fd tag. Follows the precedent
/// of `CRATONVM_DBG_SC_READ` / `CRATONVM_DBG_SC_CLOSE` in `socket_channel.rs`.
fn dbg_aio_enabled() -> bool {
    io_flags().dbg_aio
}
macro_rules! dbg_aio {
    ($($arg:tt)*) => {
        if dbg_aio_enabled() {
            eprintln!("[dbg-aio {:?}] {}", std::time::Instant::now(), format!($($arg)*));
        }
    };
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
    /// Handler-form write completion: handler gets `Integer.valueOf(n)` AND
    /// the source `ByteBuffer` held at `buffer_gref` has its `position`
    /// advanced by `n` first, per `AsynchronousByteChannel.write`'s contract.
    /// The dispatcher releases the global root after delivery.
    ///
    /// AUDIT 2026-07-26 (native-io-audit): the handler form used to report a
    /// bare `IntCount`, so the source buffer was never advanced — the exact
    /// defect already fixed for the sibling Future form at `FutureOutcome::
    /// Count` (see the 2026-07-17 comment there). `Job::Write` even carried
    /// the buffer as `bb_obj` and then destructured it away with `bb_obj: _`.
    WriteCount { n: i32, buffer_gref: usize },
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
    // Release (and, for handler-less writes, apply) any buffer roots parked
    // by `Job::Write` workers.
    flush_pending_root_releases(ctx);
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
                    CompletionKind::WriteCount { n, buffer_gref } => {
                        // Advance the source buffer BEFORE invoking the
                        // handler: a conforming `completed()` re-arms with
                        // `buf.hasRemaining()`, so it must observe the
                        // consumed position. Without this the caller resends
                        // the identical slice forever (Tomcat's
                        // `WsRemoteEndpointImplBase` write loop).
                        if buffer_gref != 0 {
                            if n > 0 {
                                if let Some(bb) = ctx.resolve_global_root(buffer_gref) {
                                    let position = match ctx.get_field_by_name(bb, "position") {
                                        Value::Int(v) if v >= 0 => v,
                                        _ => 0,
                                    };
                                    ctx.set_field_by_name(
                                        bb,
                                        "position",
                                        Value::Int(position.saturating_add(n)),
                                    );
                                }
                            }
                            ctx.remove_global_root(buffer_gref);
                        }
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
                let inv = ctx.invoke(
                    "java/nio/channels/CompletionHandler",
                    "completed",
                    "(Ljava/lang/Object;Ljava/lang/Object;)V",
                    &[Value::Object(Some(c.handler)), result_val, attach],
                );
                // A `CompletionHandler.completed` that throws used to be
                // discarded silently (`let _ = ctx.invoke(..)`), which hid
                // real failures in the accept path for a long time.
                if let Err(ref e) = inv {
                    dbg_aio!("DELIVER completed threw {:?}", e);
                }
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
    /// When the worker's blocking `read` returned, i.e. when this completion
    /// became deliverable. `Some` only under `CRATONVM_DBG_AIO_INLINE`. The
    /// dispatcher subtracts it on delivery so OUR wake latency can be told
    /// apart from time spent waiting on the peer — see [`aio_latency_report`].
    ready_at: Option<std::time::Instant>,
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

/// Park a handler-form completion (accept / connect / write / immediate
/// error) and wake the VM-attached dispatcher.
///
/// FIX (sslWithHttp11Nio2Protocol): these used to be pushed straight onto
/// `completion_queue()` with no wake-up at all. That queue is drained by
/// `drain_completions`, which only runs when some other AIO native is called
/// on a VM thread — so a caller that arms one `accept()` and then simply waits
/// (Tomcat's `Nio2Endpoint`, and any idiomatic NIO2 server) never had its
/// `CompletionHandler` invoked.
fn push_handler_completion(c: Completion) {
    completion_queue().lock().push_back(c);
    // Same condvar the dispatcher parks on for read/future completions.
    read_completion_state().1.notify_one();
}

/// Park a Future-form completion and wake the VM-attached dispatcher.
fn push_future_completion(c: FutureCompletion) {
    dbg_aio!(
        "      push  future_gref={} queued for delivery",
        c.future_gref
    );
    let (q, cv) = read_completion_state();
    q.lock().push_back(DispatcherCompletion::Future(c));
    cv.notify_one();
}

/// Block up to `timeout` for at least one pending read completion. Returns
/// `true` if one is available. Called by the AIO dispatcher thread while it is
/// in the GC-blocked idle region (no `NativeContext` needed).
pub fn wait_for_pending(timeout: std::time::Duration) -> bool {
    // `completion_queue()` (accept/connect/write handler completions) is
    // checked alongside the read/future queue — see `push_handler_completion`.
    // Checked before taking `q`'s lock so the two are never held nested.
    if !completion_queue().lock().is_empty() {
        return true;
    }
    let (q, cv) = read_completion_state();
    let mut guard = q.lock();
    if !guard.is_empty() {
        return true;
    }
    cv.wait_for(&mut guard, timeout);
    if !guard.is_empty() {
        return true;
    }
    drop(guard);
    !completion_queue().lock().is_empty()
}

/// Maximum read completions delivered per dispatcher wake-up.
const READ_DRAIN_LIMIT: usize = 256;

/// Deliver pending handler-form read completions, invoking
/// `CompletionHandler.completed` / `failed` on the calling (dispatcher) thread.
/// Requires a live `NativeContext`, so it must run on a VM/attached thread.
pub fn drain_completions_pub(ctx: &mut dyn NativeContext) {
    // Handler-form accept/connect/write completions live in a separate queue
    // that used to have no dispatcher at all — see `push_handler_completion`.
    drain_completions(ctx);
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
        ready_at,
    } = c;
    if let Some(ready) = ready_at {
        // Worker parked this the instant its blocking read returned; everything
        // since is the worker→dispatcher handoff, i.e. latency we own.
        aio_latency_record(
            &AIO_DELIVER_NS,
            &AIO_DELIVER_N,
            std::time::Instant::now().saturating_duration_since(ready),
        );
    }
    dbg_aio!(
        "HREAD  deliver handler_gref={handler_gref} outcome={}",
        match &outcome {
            ReadOutcome::Bytes(b) => format!("Bytes(len={})", b.len()),
            ReadOutcome::Eof => "Eof".to_string(),
            ReadOutcome::Count(n) => format!("Count({n})"),
            ReadOutcome::Error(m) => format!("Error({m})"),
        }
    );
    // Nothing to deliver to if the handler root is gone; just release.
    if ctx.resolve_global_root(handler_gref).is_none() {
        dbg_aio!("HREAD  deliver handler_gref={handler_gref} — handler root GONE, dropping completion silently");
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
    dbg_aio!(
        "      deliver future_gref={future_gref} outcome={}",
        match &outcome {
            FutureOutcome::Bytes(b) => format!(
                "Bytes(len={} hex={})",
                b.len(),
                b.iter()
                    .take(32)
                    .map(|x| format!("{x:02x}"))
                    .collect::<String>()
            ),
            FutureOutcome::Count(n) => format!("Count({n})"),
            FutureOutcome::Eof => "Eof".to_string(),
            FutureOutcome::Error(m) => format!("Error({m})"),
        }
    );
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
    let invoke_result = ctx.invoke(
        "java/nio/channels/CompletionHandler",
        "completed",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        &[Value::Object(Some(h)), result_val, Value::Object(attach)],
    );
    if let Err(e) = &invoke_result {
        dbg_aio!("HREAD  CompletionHandler.completed() THREW/FAILED: {e:?}");
    }
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
        /// Global-root handle for the source `ByteBuffer`, so the dispatcher
        /// can advance its `position` by the bytes actually written (see
        /// `CompletionKind::WriteCount`). This used to be a raw `ObjectRef`
        /// (`bb_obj`) held across a worker-thread blocking write — both
        /// unrooted against a moving GC and, worse, simply discarded.
        bb_gref: usize,
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
        /// When the VM thread queued this, under `CRATONVM_DBG_AIO_INLINE`
        /// only. The worker subtracts it on pick-up to expose the queue→worker
        /// leg, which `wait` cannot see (it starts once the worker is already
        /// in `read`) and which is entirely ours.
        queued_at: Option<std::time::Instant>,
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
                        push_handler_completion(Completion {
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
                        push_handler_completion(Completion {
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
                            push_handler_completion(Completion {
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
                        push_handler_completion(Completion {
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
                        push_handler_completion(Completion {
                            handler: h,
                            attachment,
                            outcome: Ok(CompletionKind::IntCount(n as i32)),
                        });
                    }
                }
                Err(e) => {
                    if let Some(h) = handler {
                        push_handler_completion(Completion {
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
            bb_gref,
            handler,
            attachment,
        } => {
            let stream = {
                let map = aio_registry().read();
                match map.get(&id) {
                    Some(AioHandle::Stream(s)) => Arc::clone(s),
                    _ => {
                        queue_root_release(bb_gref);
                        if let Some(h) = handler {
                            push_handler_completion(Completion {
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
                        // `WriteCount` carries the buffer root so the
                        // dispatcher advances `position` by `n` before
                        // invoking `completed()`; it also releases the root.
                        push_handler_completion(Completion {
                            handler: h,
                            attachment,
                            outcome: Ok(CompletionKind::WriteCount {
                                n: n as i32,
                                buffer_gref: bb_gref,
                            }),
                        });
                    } else {
                        // No handler to deliver to — still advance the
                        // buffer (a write DID happen) and drop the root.
                        queue_write_advance(bb_gref, n as i32);
                    }
                }
                Err(e) => {
                    // A partial write before the error still consumed
                    // `written` bytes from the socket's point of view, but the
                    // JDK reports the operation as failed and leaves the
                    // buffer position unspecified; just release the root.
                    queue_root_release(bb_gref);
                    if let Some(h) = handler {
                        push_handler_completion(Completion {
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
                    Some(AioHandle::Listener(l, _)) => Arc::clone(l),
                    _ => {
                        if let Some(h) = handler {
                            push_handler_completion(Completion {
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
                        push_handler_completion(Completion {
                            handler: h,
                            attachment,
                            outcome: Ok(CompletionKind::AcceptedChannel(new_id)),
                        });
                    }
                }
                Err(e) => {
                    if let Some(h) = handler {
                        push_handler_completion(Completion {
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
            queued_at,
        } => {
            if let Some(queued) = queued_at {
                aio_latency_record(
                    &AIO_QUEUE_NS,
                    &AIO_QUEUE_N,
                    std::time::Instant::now().saturating_duration_since(queued),
                );
            }
            // Blocking read on the cloned handle. The clone is private to this
            // worker, so we hold its lock for the duration without blocking the
            // application's writes (which go through the original fd entry).
            dbg_aio!(
                "HREAD  worker start  handler_gref={handler_gref} requested_len={len} thread={:?}",
                std::thread::current().id()
            );
            let mut buf = vec![0u8; len.max(1)];
            let blocked_from = aio_inline_dbg_enabled().then(std::time::Instant::now);
            let read_res = {
                let s = stream.lock();
                let mut r = &*s;
                r.read(&mut buf)
            };
            let ready_at = blocked_from.map(|started| {
                let now = std::time::Instant::now();
                aio_wait_record(now.saturating_duration_since(started));
                now
            });
            dbg_aio!(
                "HREAD  worker result handler_gref={handler_gref} result={:?} thread={:?}",
                match &read_res {
                    Ok(n) => format!("Ok({n})"),
                    Err(e) => format!("Err({e})"),
                },
                std::thread::current().id()
            );
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
                ready_at,
            });
        }
        Job::ReadFutureFd {
            stream,
            len,
            future_gref,
            buffer_gref,
        } => {
            dbg_aio!(
                "READ  worker start  future_gref={future_gref} requested_len={len} thread={:?}",
                std::thread::current().id()
            );
            let mut buf = vec![0u8; len.max(1)];
            let read_res = {
                let s = stream.lock();
                let mut r = &*s;
                r.read(&mut buf)
            };
            dbg_aio!(
                "READ  worker result future_gref={future_gref} result={:?} thread={:?}",
                match &read_res {
                    Ok(n) => format!("Ok({n})"),
                    Err(e) => format!("Err({e})"),
                },
                std::thread::current().id()
            );
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
            dbg_aio!(
                "WRITE worker start  future_gref={future_gref} data_len={} thread={:?}",
                data.len(),
                std::thread::current().id()
            );
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
            dbg_aio!(
                "WRITE worker result future_gref={future_gref} result={:?} thread={:?}",
                match &write_res {
                    Ok(n) => format!("Ok({n})"),
                    Err(e) => format!("Err({e})"),
                },
                std::thread::current().id()
            );
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

/// A source-`ByteBuffer` global root parked by a worker for the next
/// user-thread drain. `advance > 0` means "bump `position` by this many bytes
/// first"; `0` means "just drop the root".
///
/// AUDIT 2026-07-26 (native-io-audit): needed because `Job::Write` now holds
/// its source buffer as a global root (it previously held a bare, unrooted
/// `ObjectRef` that it discarded). Workers have no `&mut NativeContext`, so
/// the release has to be parked — same shape as `PendingFieldReset` above.
/// Every `Job::Write` exit path must reach exactly one of `WriteCount`,
/// `queue_write_advance`, or `queue_root_release`, or the root leaks.
struct PendingRootRelease {
    gref: usize,
    advance: i32,
}

fn pending_root_releases() -> &'static Mutex<Vec<PendingRootRelease>> {
    static V: OnceLock<Mutex<Vec<PendingRootRelease>>> = OnceLock::new();
    V.get_or_init(|| Mutex::new(Vec::new()))
}

/// Park a global root for release on the next user-thread drain.
fn queue_root_release(gref: usize) {
    if gref != 0 {
        pending_root_releases()
            .lock()
            .push(PendingRootRelease { gref, advance: 0 });
    }
}

/// Park a buffer-position advance + root release (handler-less write).
fn queue_write_advance(gref: usize, advance: i32) {
    if gref != 0 {
        pending_root_releases()
            .lock()
            .push(PendingRootRelease { gref, advance });
    }
}

fn flush_pending_root_releases(ctx: &mut dyn NativeContext) {
    let parked = std::mem::take(&mut *pending_root_releases().lock());
    for r in parked {
        if r.advance > 0 {
            if let Some(bb) = ctx.resolve_global_root(r.gref) {
                let position = match ctx.get_field_by_name(bb, "position") {
                    Value::Int(v) if v >= 0 => v,
                    _ => 0,
                };
                ctx.set_field_by_name(bb, "position", Value::Int(position.saturating_add(r.advance)));
            }
        }
        ctx.remove_global_root(r.gref);
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
        if dbg_aio_enabled() {
            let raw_id = if ctx.object_num_fields(this) > F_REG_ID {
                match ctx.get_field(this, F_REG_ID) {
                    Value::Int(v) => v,
                    _ => i32::MIN,
                }
            } else {
                i32::MIN
            };
            dbg_aio!("CLOSE entered, raw F_REG_ID field={raw_id}");
        }
        if ctx.object_num_fields(this) > F_OPEN {
            ctx.set_field(this, F_OPEN, Value::Int(0));
        }
        if ctx.object_num_fields(this) > F_CONNECTED {
            ctx.set_field(this, F_CONNECTED, Value::Int(0));
        }
        if let Some(id) = read_aio_id(ctx, this) {
            dbg_aio!(
                "CLOSE aio id={id} — shutting down socket to unblock any in-flight clone reads"
            );
            if (id as i64) < AIO_REG_BASE {
                // fd_table-backed channel (Future-form connect path).
                aio_shutdown_fd_table_stream(ctx, id as u32);
            } else {
                aio_shutdown_stream(id);
                aio_remove(id);
            }
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

/// Decode the `(long timeout, TimeUnit unit)` pair of the timed
/// `read`/`write` overloads into a `Duration`.
///
/// `None` (block indefinitely) for a non-positive timeout, for `Long.MAX_VALUE`
/// and other absurd values, and whenever the `TimeUnit` cannot be consulted —
/// matching the untimed overloads' behaviour, which is the safe default.
fn aio_timeout_from_args(
    ctx: &mut dyn NativeContext,
    timeout: Option<&Value>,
    unit: Option<&Value>,
) -> Option<Duration> {
    let raw = match timeout {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => return None,
    };
    if raw <= 0 {
        return None;
    }
    let millis = match unit {
        Some(Value::Object(Some(u))) => {
            match ctx.invoke_virtual(*u, "toMillis", "(J)J", &[Value::Long(raw)]) {
                Ok(Some(Value::Long(ms))) => ms,
                Ok(Some(Value::Int(ms))) => ms as i64,
                // Unknown unit: treat the value as milliseconds, which is what
                // every caller in practice passes.
                _ => raw,
            }
        }
        _ => raw,
    };
    // Guard against `Long.MAX_VALUE`-style "no timeout" sentinels overflowing
    // `Duration` or setting a socket option the OS rejects.
    if millis <= 0 || millis > 24 * 60 * 60 * 1000 {
        return None;
    }
    Some(Duration::from_millis(millis as u64))
}

/// An independent `try_clone()` handle on whichever socket backs an
/// `AsynchronousSocketChannel`'s slot-2 value: an fd_table fd (channel from the
/// Future-form `connect`) below `AIO_REG_BASE`, or an `aio_registry` id at or
/// above it (channel handed to `AsynchronousServerSocketChannel.accept`'s
/// `CompletionHandler`).
///
/// The read/write natives all used to accept only the fd_table form, so every
/// operation on an ACCEPTED channel failed instantly with "not connected" and
/// no NIO2 *server* could serve a byte — Tomcat's `Http11Nio2Protocol` drives
/// its `SecureNio2Channel` TLS handshake through these calls
/// (`TomcatServletWebServerFactoryTests.sslWithHttp11Nio2Protocol`).
///
/// A clone (rather than the shared handle) keeps a blocking read from
/// contending with concurrent writes on the same connection.
fn aio_clone_backing_stream(
    ctx: &dyn NativeContext,
    slot2: i32,
) -> Result<TcpStream, std::io::Error> {
    if (slot2 as i64) < AIO_REG_BASE {
        return ctx.fd_table().try_clone_tcp(slot2 as u32);
    }
    let handle = match aio_registry().read().get(&slot2) {
        Some(AioHandle::Stream(s)) => Some(Arc::clone(s)),
        _ => None,
    };
    match handle {
        Some(s) => {
            let guard = s.lock();
            guard.try_clone()
        }
        None => Err(std::io::Error::new(
            ErrorKind::NotConnected,
            "channel closed",
        )),
    }
}

thread_local! {
    /// Depth of the inline read-completion chain on this thread (see
    /// [`try_deliver_ready_read`]). A `CompletionHandler` normally arms the
    /// next read from inside `completed()`, so delivering inline would
    /// otherwise recurse for as long as the peer keeps the socket readable and
    /// grow the Java stack without bound.
    static INLINE_READY_READ_DEPTH: Cell<u8> = const { Cell::new(0) };
}

/// How many reads one thread may complete inline before falling back to the
/// worker pool. Two is enough: a completion handler normally arms exactly one
/// follow-on read, so this collapses the handoff for the "next frame is already
/// buffered" case while keeping the recursion trivially bounded.
///
/// Raising it does nothing, which is worth recording so it is not retried.
/// `CRATONVM_DBG_AIO_INLINE=1` on `TestAsyncMessagesPerformance`:
///
/// ```text
/// cap  2: reads=1500 inline=982  not_ready=27  depth_capped=491
/// cap 16: reads=1500 inline=1002 not_ready=498 depth_capped=0
/// ```
///
/// The inline rate barely moves (0.655 → 0.668) and SEQ2 is unchanged, because
/// the reads the cap was turning away are the SAME reads that have no data:
/// the third read of the test's 8k/8k/4k cycle is issued just before the
/// server's 50 ms pause. The old `depth_capped=491` was an artefact of this
/// counter checking the cap before readiness — see [`try_deliver_ready_read`],
/// which now probes readiness first so the two are attributed correctly.
const INLINE_READY_READ_MAX_DEPTH: u8 = 2;

/// Complete a handler-form read on the calling thread when the socket is
/// already readable, instead of handing it to a worker.
///
/// The normal path costs two thread handoffs per read — the calling thread
/// queues a `Job::ReadFd`, a pool worker wakes and blocks in `recv`, then the
/// dispatcher wakes to run the Java `CompletionHandler`. For a WebSocket
/// conversation on loopback the bytes are usually already in the kernel receive
/// buffer by the time the handler arms its next read, so both wakes are pure
/// latency. `TestAsyncMessagesPerformance` asserts that consecutive chunks of
/// one message arrive < 0.5 ms apart and measured 0.6-1.1 ms
/// (known-issue tomcat/32.3).
///
/// Delivering a completion on the initiating thread is explicitly permitted by
/// `AsynchronousChannelGroup` ("the completion handler may be invoked directly
/// by the initiating thread" when the operation completes immediately), and
/// Tomcat's `Nio2Endpoint` already handles it via its inline-completion guard.
///
/// Only taken when `FIONREAD` proves the read cannot block; a `WouldBlock`
/// raced in anyway simply returns `false` and the caller queues the job as
/// before. Returns `true` only when the completion has actually been delivered.
fn try_deliver_ready_read(
    ctx: &mut dyn NativeContext,
    stream: &mut TcpStream,
    length: usize,
    handler: ObjectRef,
    attachment: Option<ObjectRef>,
    buffer: ObjectRef,
) -> bool {
    // Probe readiness BEFORE the depth check, so a read that is both capped and
    // has no data is attributed to "not ready" rather than to the cap. Getting
    // this order wrong reported 491 depth-capped reads that were really just
    // empty, and sent an earlier round of this work chasing the cap.
    if socket_ready_bytes(stream) == 0 {
        if aio_inline_dbg_enabled() {
            aio_inline_record(&AIO_INLINE_NOT_READY);
        }
        return false;
    }
    let entered = INLINE_READY_READ_DEPTH.with(|depth| {
        if depth.get() >= INLINE_READY_READ_MAX_DEPTH {
            false
        } else {
            depth.set(depth.get() + 1);
            true
        }
    });
    if !entered {
        if aio_inline_dbg_enabled() {
            aio_inline_record(&AIO_INLINE_DEPTH_CAPPED);
        }
        return false;
    }

    // Shared with the Future form. An `AsynchronousSocketChannel` permits only
    // one outstanding read at a time, so no other consumer can drain the socket
    // between the `FIONREAD` probe and the read.
    let outcome = try_read_ready_bytes(stream, length).map(|o| match o {
        FutureOutcome::Bytes(bytes) => ReadOutcome::Bytes(bytes),
        FutureOutcome::Eof => ReadOutcome::Eof,
        FutureOutcome::Count(n) => ReadOutcome::Count(n),
        FutureOutcome::Error(m) => ReadOutcome::Error(m),
    });
    let delivered = outcome.is_some();
    if let Some(outcome) = outcome {
        // `deliver_read_completion` releases these roots itself.
        let handler_gref = ctx.add_global_root(handler);
        let attachment_gref = attachment.map(|a| ctx.add_global_root(a)).unwrap_or(0);
        let buffer_gref = ctx.add_global_root(buffer);
        deliver_read_completion(
            ctx,
            ReadCompletion {
                handler_gref,
                attachment_gref,
                buffer_gref,
                outcome,
                // Delivered on the initiating thread — there is no handoff to
                // measure, and counting it would dilute the worker-path mean.
                ready_at: None,
            },
        );
    }
    INLINE_READY_READ_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    delivered
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
        Value::Int(v) if v >= 0 => v,
        _ => return post(FutureOutcome::Error("read: not connected".to_string())),
    };
    let (_, _, _, length) = decode_buffer(ctx, bb);
    dbg_aio!("READ  dispatch fd={fd} requested_len={length} future_gref={future_gref}");
    if length <= 0 {
        return post(FutureOutcome::Count(0));
    }
    let mut stream = match aio_clone_backing_stream(ctx, fd) {
        Ok(stream) => stream,
        Err(error) => return post(FutureOutcome::Error(format!("read: {error}"))),
    };

    // Already-buffered bytes complete the Future on this thread, so
    // `Future.get()` returns without a worker wake OR a dispatcher wake. This
    // is the path Tomcat's WebSocket CLIENT takes: `AsyncChannelWrapperNonSecure`
    // reads through the Future form, not the handler form, which is why the
    // handler-form fast path alone left known-issue tomcat/32.3's SEQ1
    // assertion (the gap between the two 8k chunks of one 16k message)
    // essentially unchanged at 494 failures out of 500.
    //
    // Unlike the handler form there is no re-entrancy to bound: completing the
    // Future cannot run application code here, because the Future has not been
    // returned to Java yet and so carries no dependent stages.
    if let Some(outcome) = try_read_ready_bytes(&mut stream, length as usize) {
        deliver_future_completion(
            ctx,
            FutureCompletion {
                future_gref,
                buffer_gref,
                outcome,
            },
        );
        return Ok(Some(Value::Object(Some(future))));
    }

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

/// `CRATONVM_DBG_AIO_INLINE=1` reports how often the ready-read fast path is
/// actually taken, split by why it declined. Without this it is impossible to
/// tell "the fast path did not help" from "the fast path never ran" — the two
/// call for opposite next steps.
fn aio_inline_dbg_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(
        || match cratonvm_types::flags::runtime_var("CRATONVM_DBG_AIO_INLINE") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => false,
        },
    )
}

static AIO_INLINE_TAKEN: AtomicUsize = AtomicUsize::new(0);
static AIO_INLINE_NOT_READY: AtomicUsize = AtomicUsize::new(0);
static AIO_INLINE_DEPTH_CAPPED: AtomicUsize = AtomicUsize::new(0);

/// Split of where a NOT-READY read's latency goes, under
/// `CRATONVM_DBG_AIO_INLINE`.
///
/// `try_deliver_ready_read` only helps reads whose bytes have already arrived.
/// For the rest the caller queues a `Job::ReadFd`, a worker blocks in `read`,
/// and the dispatcher then wakes to run the Java `CompletionHandler` — so the
/// observed gap is `wait` (genuinely waiting on the peer, not ours to fix) plus
/// `deliver` (worker→dispatcher handoff, which is). Without the split, a gap
/// dominated by peer turnaround is indistinguishable from one dominated by our
/// own wake latency, and tomcat/32.3's SEQ2 could not be attributed to either.
/// `wait` is BIMODAL whenever the peer alternates bursts with pauses, so a mean
/// over it answers nothing: `TestAsyncMessagesPerformance` has one read per
/// cycle waiting out a deliberate 50 ms pause and one waiting ~1 ms for the
/// next message, and their mean (~25 ms) is a number no read ever experienced.
/// Bucketing separates them. `queue` is the third term the mean hid entirely —
/// time from the VM thread queueing `Job::ReadFd` to a worker actually
/// entering `read`, which is ours and is invisible in `wait`.
static AIO_WAIT_NS: AtomicUsize = AtomicUsize::new(0);
static AIO_WAIT_N: AtomicUsize = AtomicUsize::new(0);
static AIO_WAIT_LT1MS: AtomicUsize = AtomicUsize::new(0);
static AIO_WAIT_1_10MS: AtomicUsize = AtomicUsize::new(0);
static AIO_WAIT_GT10MS: AtomicUsize = AtomicUsize::new(0);
static AIO_WAIT_SHORT_NS: AtomicUsize = AtomicUsize::new(0);
static AIO_QUEUE_NS: AtomicUsize = AtomicUsize::new(0);
static AIO_QUEUE_N: AtomicUsize = AtomicUsize::new(0);
static AIO_DELIVER_NS: AtomicUsize = AtomicUsize::new(0);
static AIO_DELIVER_N: AtomicUsize = AtomicUsize::new(0);

fn aio_latency_record(
    total: &AtomicUsize,
    count: &AtomicUsize,
    d: std::time::Duration,
) {
    total.fetch_add(d.as_nanos().min(usize::MAX as u128) as usize, Ordering::Relaxed);
    let n = count.fetch_add(1, Ordering::Relaxed) + 1;
    if n % 500 == 0 {
        aio_latency_report();
    }
}

fn aio_wait_record(d: std::time::Duration) {
    let ns = d.as_nanos().min(usize::MAX as u128) as usize;
    AIO_WAIT_NS.fetch_add(ns, Ordering::Relaxed);
    if ns < 1_000_000 {
        AIO_WAIT_LT1MS.fetch_add(1, Ordering::Relaxed);
        AIO_WAIT_SHORT_NS.fetch_add(ns, Ordering::Relaxed);
    } else if ns < 10_000_000 {
        AIO_WAIT_1_10MS.fetch_add(1, Ordering::Relaxed);
        AIO_WAIT_SHORT_NS.fetch_add(ns, Ordering::Relaxed);
    } else {
        AIO_WAIT_GT10MS.fetch_add(1, Ordering::Relaxed);
    }
    let n = AIO_WAIT_N.fetch_add(1, Ordering::Relaxed) + 1;
    if n % 500 == 0 {
        aio_latency_report();
    }
}

fn aio_latency_report() {
    let mean = |ns: &AtomicUsize, n: &AtomicUsize| {
        let c = n.load(Ordering::Relaxed);
        if c == 0 {
            0
        } else {
            ns.load(Ordering::Relaxed) / c / 1000
        }
    };
    let lt1 = AIO_WAIT_LT1MS.load(Ordering::Relaxed);
    let m1_10 = AIO_WAIT_1_10MS.load(Ordering::Relaxed);
    let gt10 = AIO_WAIT_GT10MS.load(Ordering::Relaxed);
    let short_n = lt1 + m1_10;
    let short_mean = if short_n == 0 {
        0
    } else {
        AIO_WAIT_SHORT_NS.load(Ordering::Relaxed) / short_n / 1000
    };
    eprintln!(
        "[DBG_AIO_LATENCY] not_ready_reads n={} | wait buckets: <1ms={} 1-10ms={} >10ms={} \
         (sub-10ms mean={}us) | queue_mean={}us (n={}) deliver_mean={}us (n={})",
        AIO_WAIT_N.load(Ordering::Relaxed),
        lt1,
        m1_10,
        gt10,
        short_mean,
        mean(&AIO_QUEUE_NS, &AIO_QUEUE_N),
        AIO_QUEUE_N.load(Ordering::Relaxed),
        mean(&AIO_DELIVER_NS, &AIO_DELIVER_N),
        AIO_DELIVER_N.load(Ordering::Relaxed),
    );
}

fn aio_inline_record(counter: &AtomicUsize) {
    counter.fetch_add(1, Ordering::Relaxed);
    let taken = AIO_INLINE_TAKEN.load(Ordering::Relaxed);
    let not_ready = AIO_INLINE_NOT_READY.load(Ordering::Relaxed);
    let capped = AIO_INLINE_DEPTH_CAPPED.load(Ordering::Relaxed);
    let total = taken + not_ready + capped;
    if total % 500 == 0 {
        eprintln!(
            "[DBG_AIO_INLINE] reads={total} inline={taken} not_ready={not_ready} \
             depth_capped={capped} inline_rate={:.3}",
            taken as f64 / total as f64
        );
    }
}

/// Read without blocking when `FIONREAD` proves bytes are already queued.
///
/// Shared by the handler and Future read fast paths. `None` means "not ready,
/// use the worker pool" — including the `WouldBlock` that a racing consumer
/// could still produce — so every caller keeps its existing asynchronous
/// behaviour whenever this declines.
/// Bytes `FIONREAD` says are queued, clamped to a `usize`. `0` means "would
/// block" for the purposes of the ready-read fast paths.
fn socket_ready_bytes(stream: &TcpStream) -> usize {
    crate::net::socket_available_stream(stream)
        .unwrap_or(0)
        .max(0) as usize
}

fn try_read_ready_bytes(stream: &mut TcpStream, length: usize) -> Option<FutureOutcome> {
    if length == 0 {
        return None;
    }
    let available = socket_ready_bytes(stream);
    if available == 0 {
        if aio_inline_dbg_enabled() {
            aio_inline_record(&AIO_INLINE_NOT_READY);
        }
        return None;
    }
    let mut bytes = vec![0u8; available.min(length)];
    let outcome = match stream.read(&mut bytes) {
        Ok(n) if n > 0 => {
            bytes.truncate(n);
            Some(FutureOutcome::Bytes(bytes))
        }
        Ok(_) => Some(FutureOutcome::Eof),
        Err(e) if e.kind() == ErrorKind::WouldBlock => None,
        Err(e) => Some(FutureOutcome::Error(format!("read failed: {e}"))),
    };
    if aio_inline_dbg_enabled() {
        aio_inline_record(if outcome.is_some() {
            &AIO_INLINE_TAKEN
        } else {
            &AIO_INLINE_NOT_READY
        });
    }
    outcome
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
        Value::Int(v) if v >= 0 => v,
        _ => return post(FutureOutcome::Error("write: not connected".to_string())),
    };
    let data = read_buffer_bytes(ctx, bb);
    dbg_aio!(
        "WRITE dispatch fd={fd} data_len={} future_gref={future_gref} hex={}",
        data.len(),
        data.iter()
            .take(32)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    if data.is_empty() {
        return post(FutureOutcome::Count(0));
    }
    let stream = match aio_clone_backing_stream(ctx, fd) {
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
    // Two shapes share this native: the untimed
    // `read(ByteBuffer, A, CompletionHandler)` and the timed
    // `read(ByteBuffer, long, TimeUnit, A, CompletionHandler)` — Tomcat's
    // `SecureNio2Channel` uses only the timed one.
    let timed = args.len() >= 6;
    let (attachment, handler_arg) = if timed {
        (obj_or_none(args, 4), obj_or_none(args, 5))
    } else {
        (obj_or_none(args, 2), obj_or_none(args, 3))
    };
    let read_timeout = if timed {
        aio_timeout_from_args(ctx, args.get(2), args.get(3))
    } else {
        None
    };
    let handler = match handler_arg {
        Some(h) => h,
        // No CompletionHandler ⇒ nothing to deliver (the Future-form read is a
        // separate native registered elsewhere).
        None => return Ok(Some(Value::Object(None))),
    };
    dbg_aio!(
        "HREAD dispatch (handler-form) this_fields={}",
        ctx.object_num_fields(this)
    );

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
            ready_at: None,
        });
    };

    // The channel was connected via the Future-form `connect`, which stores the
    // Slot 2 is either an fd_table fd (channel produced by the Future-form
    // `connect` — the client path this native was originally written for) or,
    // for a channel handed to `AsynchronousServerSocketChannel.accept`'s
    // `CompletionHandler`, an `aio_registry` id >= `AIO_REG_BASE`. Only the
    // former used to be accepted, so EVERY server-side read failed with
    // "read: not connected" and no NIO2 server could serve a byte
    // (`TomcatServletWebServerFactoryTests.sslWithHttp11Nio2Protocol`).
    let slot2 = match ctx.get_field(this, 2) {
        Value::Int(v) if v >= 0 => v,
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

    // Independent read handle either way, so the blocking read does not
    // contend with the application's concurrent writes on the same socket.
    let mut stream = match aio_clone_backing_stream(ctx, slot2) {
        Ok(s) => s,
        Err(e) => {
            post_immediate(ctx, ReadOutcome::Error(format!("read: {e}")));
            return Ok(Some(Value::Object(None)));
        }
    };
    // Honour the timed overload's deadline on the worker's private handle.
    if let Some(d) = read_timeout {
        let _ = stream.set_read_timeout(Some(d));
    }

    // Already-buffered bytes complete without ever reaching the pool.
    if try_deliver_ready_read(ctx, &mut stream, length as usize, handler, attachment, bb) {
        return Ok(Some(Value::Object(None)));
    }

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
            queued_at: aio_inline_dbg_enabled().then(std::time::Instant::now),
        })
        .is_err()
    {
        // Pool gone: report failure to the handler (roots already taken).
        push_read_completion(ReadCompletion {
            handler_gref,
            attachment_gref,
            buffer_gref,
            outcome: ReadOutcome::Error("read: aio worker pool unavailable".to_string()),
            ready_at: None,
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
    // Untimed `write(ByteBuffer, A, CompletionHandler)` and timed
    // `write(ByteBuffer, long, TimeUnit, A, CompletionHandler)` share this
    // native — see the matching comment in `aio_asc_read`.
    let (attachment, handler) = if args.len() >= 6 {
        (obj_or_none(args, 4), obj_or_none(args, 5))
    } else {
        (obj_or_none(args, 2), obj_or_none(args, 3))
    };
    let id = read_aio_id(ctx, this).ok_or_else(|| ioex("write: not connected"))?;
    let data = read_buffer_bytes(ctx, bb);
    if data.is_empty() {
        if let Some(h) = handler {
            push_handler_completion(Completion {
                handler: h,
                attachment,
                outcome: Ok(CompletionKind::IntCount(0)),
            });
        }
        return Ok(Some(Value::Object(None)));
    }
    // Root the source buffer for the duration of the worker write: the
    // dispatcher must advance its `position` by the bytes actually written
    // (`AsynchronousByteChannel.write` contract), and a moving collection can
    // run while the worker is parked in `write(2)`.
    let bb_gref = ctx.add_global_root(bb);
    if let Err(e) = job_sender().send(Job::Write {
        id,
        data,
        bb_gref,
        handler,
        attachment,
    }) {
        ctx.remove_global_root(bb_gref);
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

/// `NetworkChannel.setOption` on an async channel. Real JDK dispatch would
/// land on the abstract declaration (no Code attribute) and throw
/// `AbstractMethodError` — see this module's `aio_set_option` doc and
/// `socket_channel.rs::supported_socket_options` for the same fix on the
/// blocking channels. TCP_NODELAY is honoured on the backing socket; the
/// others are accepted no-ops.
fn aio_set_option(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let opt_name = match obj_or_none(args, 1) {
        Some(o) => ctx
            .invoke_virtual(o, "name", "()Ljava/lang/String;", &[])
            .ok()
            .flatten()
            .and_then(|v| match v {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            })
            .unwrap_or_default(),
        None => String::new(),
    };
    if opt_name == "TCP_NODELAY" {
        let on = match args.get(2) {
            Some(Value::Object(Some(b))) => {
                !matches!(ctx.get_field_by_name(*b, "value"), Value::Int(0))
            }
            Some(Value::Int(v)) => *v != 0,
            _ => true,
        };
        if let Some(id) = read_aio_id(ctx, this) {
            if (id as i64) < AIO_REG_BASE {
                let _ = ctx.fd_table().tcp_set_nodelay(id as u32, on);
            } else if let Some(AioHandle::Stream(s)) = aio_registry().read().get(&id) {
                let _ = s.lock().set_nodelay(on);
            }
        }
    }
    // Covariant return: the caller's checkcast expects the channel back.
    Ok(Some(Value::Object(Some(this))))
}

/// `NetworkChannel.getOption` — same abstract-method problem as
/// `aio_set_option`. Only TCP_NODELAY has a real answer; everything else
/// reports `null`, which callers treat as "not configured".
fn aio_get_option(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let _ = ctx;
    Ok(Some(Value::Object(None)))
}

fn aio_supported_options(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    crate::socket_channel::supported_socket_options_pub(ctx)
}

/// `AsynchronousSocketChannel.getRemoteAddress()` — Tomcat's
/// `SocketWrapperBase.populateRemoteAddr` calls it on every accepted channel.
fn aio_asc_remote_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let id = match read_aio_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    let peer = if (id as i64) < AIO_REG_BASE {
        ctx.fd_table().tcp_peer_addr(id as u32).ok()
    } else {
        match aio_registry().read().get(&id) {
            Some(AioHandle::Stream(s)) => s.lock().peer_addr().ok().map(|a| a.to_string()),
            _ => None,
        }
    };
    let Some(peer) = peer else {
        return Ok(Some(Value::Object(None)));
    };
    let (host, port) = match peer.rsplit_once(':') {
        Some((h, p)) => (
            h.trim_start_matches('[').trim_end_matches(']').to_string(),
            p.parse::<i32>().unwrap_or(0),
        ),
        None => (peer.clone(), 0),
    };
    let host_obj = ctx.create_string(&host);
    ctx.new_object_initialized(
        "java/net/InetSocketAddress",
        "(Ljava/lang/String;I)V",
        &[Value::Object(Some(host_obj)), Value::Int(port)],
    )
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
    let local_addr = listener
        .local_addr()
        .map_err(|e| ioex(format!("bind {bind_text}: local_addr: {e}")))?;
    let id = aio_register(AioHandle::Listener(
        Arc::new(Mutex::new(listener)),
        local_addr,
    ));
    if ctx.object_num_fields(this) > F_REG_ID {
        ctx.set_field(this, F_REG_ID, Value::Int(id));
    }
    Ok(Some(Value::Object(Some(this))))
}

/// `SocketAddress getLocalAddress()`. Without this, Tomcat's
/// `Nio2Endpoint.getLocalPort()` (via
/// `((InetSocketAddress) serverSock.getLocalAddress()).getPort()`) NPEs on
/// the missing native — same shape as `ssc_local_address` in
/// `socket_channel.rs`'s fix for the synchronous `ServerSocketChannel`
/// (see its comment for the full story: real JDK bytecode expects a real
/// `InetSocketAddress` with a populated `holder`, not a synthetic stub).
///
/// Reads the address captured on `AioHandle::Listener` at bind time —
/// deliberately NOT `listener.lock().local_addr()`: the accept worker
/// holds that same `Mutex` for the entire duration of its blocking
/// `TcpListener::accept()` call (see `Job::Accept`), so locking it here
/// raced the just-started accept loop and could block this call
/// indefinitely (observed as a full-suite-timeout HANG on
/// `TomcatServletWebServerFactoryTests.sslWithHttp11Nio2Protocol`, which
/// calls this immediately after `start()`).
fn aio_assc_local_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let id = match read_aio_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    let addr = match aio_registry().read().get(&id) {
        Some(AioHandle::Listener(_, addr)) => Some(*addr),
        _ => None,
    };
    let addr = match addr {
        Some(a) => a,
        None => return Ok(Some(Value::Object(None))),
    };
    // Mirror `new_resolved_inet_socket_address` in `socket_channel.rs`
    // (go through the real `InetAddress.getByName` + `InetSocketAddress
    // (InetAddress,int)` ctor, not the `(String,int)` overload) so the
    // resulting object's `holder` is populated the same proven way that
    // fixed the identical `getLocalPort()` NPE for the synchronous
    // `ServerSocketChannel`.
    let host_str = ctx.create_string(&addr.ip().to_string());
    if let Ok(Some(Value::Object(Some(inet_addr)))) = ctx.invoke(
        "java/net/InetAddress",
        "getByName",
        "(Ljava/lang/String;)Ljava/net/InetAddress;",
        &[Value::Object(Some(host_str))],
    ) {
        return ctx.new_object_initialized(
            "java/net/InetSocketAddress",
            "(Ljava/net/InetAddress;I)V",
            &[
                Value::Object(Some(inet_addr)),
                Value::Int(addr.port() as i32),
            ],
        );
    }
    let host_str = ctx.create_string(&addr.ip().to_string());
    ctx.new_object_initialized(
        "java/net/InetSocketAddress",
        "(Ljava/lang/String;I)V",
        &[
            Value::Object(Some(host_str)),
            Value::Int(addr.port() as i32),
        ],
    )
}

fn aio_assc_accept(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Start the dispatcher on first use: an idiomatic NIO2 server arms one
    // `accept()` and then does nothing else, so without this the accept
    // completion has nobody to deliver it (see `push_handler_completion`).
    ensure_dispatcher();
    drain_completions(ctx);
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("accept: null channel")),
    };
    dbg_aio!("ACCEPT native entered (arming)");
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

// JDK-ONLY-CLASSIFY: unknown — needs census. Sockets and worker threads are
// bridge territory in principle, but not one registration in this function
// resolves to an ACC_NATIVE method in JDK 25: 16 target ABSTRACT methods on
// `java.nio.channels.AsynchronousSocketChannel` / `AsynchronousServerSocketChannel`
// / `AsynchronousChannelGroup`, 10 shadow concrete bytecode, 9 name methods the
// real classes do not declare. The genuine syscall boundary for async I/O in
// JDK 25 lives one layer down in the `sun.nio.ch.*` implementation classes, and
// registering on the abstract public API instead intercepts every channel
// implementation. Evidence needed: `invocations` plus the receiver classes
// actually seen, before deciding whether to move these down a layer.
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
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/AsynchronousSocketChannel;",
        aio_set_option,
    );
    r.register(
        asc,
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
        aio_get_option,
    );
    r.register(asc, "supportedOptions", "()Ljava/util/Set;", aio_supported_options);
    r.register(
        asc,
        "getRemoteAddress",
        "()Ljava/net/SocketAddress;",
        aio_asc_remote_address,
    );
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
    // Timed overloads. Tomcat's `SecureNio2Channel` uses ONLY these, so
    // without them `Http11Nio2Protocol` accepted connections and then never
    // read or wrote a byte (`sslWithHttp11Nio2Protocol`). Both handlers
    // detect the wider arg shape themselves.
    r.register(
        asc,
        "read",
        "(Ljava/nio/ByteBuffer;JLjava/util/concurrent/TimeUnit;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V",
        aio_asc_read,
    );
    r.register(
        asc,
        "write",
        "(Ljava/nio/ByteBuffer;JLjava/util/concurrent/TimeUnit;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V",
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
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/AsynchronousServerSocketChannel;",
        aio_set_option,
    );
    r.register(
        assc,
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
        aio_get_option,
    );
    r.register(assc, "supportedOptions", "()Ljava/util/Set;", aio_supported_options);
    r.register(
        assc,
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/AsynchronousServerSocketChannel;",
        aio_assc_bind,
    );
    // Two-arg overload (explicit `backlog` hint) — real JDK code (e.g.
    // Tomcat's `Nio2Endpoint.bind`) calls this form. Without this
    // registration the abstract `AsynchronousServerSocketChannel.bind(SocketAddress,int)`
    // has no concrete override, so real-JDK-mode dispatch hits it directly
    // and throws `AbstractMethodError: has no Code attribute` instead of
    // ever reaching a native. `TcpListener::bind` has no backlog knob to
    // honor (std doesn't expose one), so this reuses the same handler as
    // the single-arg form and simply ignores the extra `int` arg — same
    // behavior the single-arg overload already has.
    r.register(
        assc,
        "bind",
        "(Ljava/net/SocketAddress;I)Ljava/nio/channels/AsynchronousServerSocketChannel;",
        aio_assc_bind,
    );
    r.register(
        assc,
        "accept",
        "(Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V",
        aio_assc_accept,
    );
    r.register(
        assc,
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
        aio_assc_local_address,
    );
    // Package-private `localAddress()` — same mirrored-alias pattern as
    // `sc_local_address`'s registration in `socket_channel.rs`.
    r.register(
        assc,
        "localAddress",
        "()Ljava/net/SocketAddress;",
        aio_assc_local_address,
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use crate::test_support::{confine_test_lock, MockNativeContext};

    /// AUDIT 2026-07-26 (native-io-audit): the handler form of
    /// `AsynchronousSocketChannel.write` reported a bare `IntCount` and never
    /// advanced the source `ByteBuffer` — the same defect already fixed for
    /// the Future form (see `FutureOutcome::Count`). A conforming caller
    /// (`while (buf.hasRemaining()) channel.write(buf, att, handler)`,
    /// re-armed from `completed()`) therefore resubmitted the identical slice
    /// forever, physically resending the frame on the wire.
    #[test]
    fn audit_handler_write_completion_advances_source_buffer() {
        let _g = confine_test_lock().lock();
        let mut ctx = MockNativeContext::new();
        completion_queue().lock().clear();

        let bb = ctx.alloc_object(8);
        ctx.set_field_by_name(bb, "position", Value::Int(5));
        let buffer_gref = ctx.add_global_root(bb);
        let handler = ctx.alloc_object(1);

        push_handler_completion(Completion {
            handler,
            attachment: None,
            outcome: Ok(CompletionKind::WriteCount {
                n: 7,
                buffer_gref,
            }),
        });
        drain_completions(&mut ctx);

        assert_eq!(
            ctx.get_field_by_name(bb, "position"),
            Value::Int(12),
            "position must advance by the bytes actually written"
        );
        assert_eq!(
            ctx.global_root_count(),
            0,
            "the buffer's global root must be released after delivery"
        );
    }

    /// The worker-parked release path (handler-less write, and every error
    /// exit) must also drop the root — otherwise every async write leaks one.
    #[test]
    fn audit_parked_root_release_advances_then_frees() {
        let _g = confine_test_lock().lock();
        let mut ctx = MockNativeContext::new();
        pending_root_releases().lock().clear();

        let advanced = ctx.alloc_object(8);
        ctx.set_field_by_name(advanced, "position", Value::Int(2));
        let g1 = ctx.add_global_root(advanced);
        queue_write_advance(g1, 3);

        let untouched = ctx.alloc_object(8);
        ctx.set_field_by_name(untouched, "position", Value::Int(9));
        let g2 = ctx.add_global_root(untouched);
        queue_root_release(g2);

        assert_eq!(ctx.global_root_count(), 2);
        flush_pending_root_releases(&mut ctx);

        assert_eq!(ctx.get_field_by_name(advanced, "position"), Value::Int(5));
        assert_eq!(
            ctx.get_field_by_name(untouched, "position"),
            Value::Int(9),
            "a failed write must not advance the buffer"
        );
        assert_eq!(ctx.global_root_count(), 0, "both roots must be freed");
    }

    /// A zero handle is the "no root" sentinel and must never be queued.
    #[test]
    fn audit_zero_gref_is_not_queued() {
        let _g = confine_test_lock().lock();
        pending_root_releases().lock().clear();
        queue_root_release(0);
        queue_write_advance(0, 4);
        assert!(pending_root_releases().lock().is_empty());
    }

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
