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
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
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

// ---------------------------------------------------------------------------
// Close-awareness for worker-thread blocking I/O (2026-08-12, W7-53)
// ---------------------------------------------------------------------------
//
// Two of the five worker arms below can re-ask `aio_registry` directly, because
// they carry the registry `id`: `Job::Accept` and `Job::Write`.
//
// The other three cannot, and their own comment says why — "the clone is
// private to this worker". `Job::ReadFd`, `Job::ReadFutureFd` and
// `Job::WriteFutureFd` hold a `try_clone()`d `TcpStream` with no id attached,
// so there is no registry question for them to ask.
//
// The file already had a mechanism aimed at exactly this, and it is only half a
// mechanism: `aio_shutdown_stream` / `aio_shutdown_fd_table_stream` issue
// `shutdown(Both)` on the shared socket before removing the entry, and their
// doc comments state without qualification that this "unblocks *every* fd that
// still references the same open-file-description". That is true on Linux,
// where `SHUT_RD` wakes a parked `recv` with EOF. It is **false on Windows**,
// where no `shutdown` aborts a pending blocking call — only `closesocket` does,
// and this code cannot close a handle a worker is mid-syscall on. The claim is
// load-bearing: it is why nothing else was ever added. So the shutdown stays
// (it is the cheaper wakeup where it works) and a cancellation flag is added
// beside it for where it does not.
//
// The flag is a per-channel `Arc<AtomicBool>` handed to the job at queue time.
// Close sets it and REMOVES the map entry; the worker still holds its `Arc` and
// sees the `true`. That keeps the map sized by live channels rather than by
// every channel the VM has ever closed, which a set of cancelled keys would
// not.

/// Cancellation key for a channel living in [`aio_registry`].
fn aio_cancel_key_registry(id: i32) -> i64 {
    i64::from(id)
}

/// Cancellation key for a channel whose connection is an `fd_table` fd (the
/// Future-form `connect` path — see `aio_shutdown_fd_table_stream`). Negated so
/// the two id spaces cannot collide.
fn aio_cancel_key_fd(fd: u32) -> i64 {
    -(i64::from(fd) + 1)
}

/// Cancellation key for whatever `F_REG_ID` holds. The two id spaces are told
/// apart exactly as every other consumer in this file tells them apart: a value
/// below `AIO_REG_BASE` is an `fd_table` fd, at or above it an `aio_registry`
/// id. Getting this wrong would hand a worker a flag nobody ever sets, which is
/// the vacuous shape -- a site that looks fixed and cannot wake.
fn aio_cancel_key_for_reg_id(v: i32) -> i64 {
    if (v as i64) < AIO_REG_BASE {
        aio_cancel_key_fd(v as u32)
    } else {
        aio_cancel_key_registry(v)
    }
}

fn aio_cancel_flags() -> &'static RwLock<HashMap<i64, Arc<AtomicBool>>> {
    static FLAGS: OnceLock<RwLock<HashMap<i64, Arc<AtomicBool>>>> = OnceLock::new();
    FLAGS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// The cancellation flag for `key`, creating it if this is the first in-flight
/// operation on that channel. Called on the VM thread at queue time, never from
/// a worker.
fn aio_cancel_flag(key: i64) -> Arc<AtomicBool> {
    if let Some(existing) = aio_cancel_flags().read().get(&key) {
        return Arc::clone(existing);
    }
    let mut map = aio_cancel_flags().write();
    Arc::clone(
        map.entry(key)
            .or_insert_with(|| Arc::new(AtomicBool::new(false))),
    )
}

/// Signal every worker parked on `key` that its channel has been closed, and
/// drop the map entry. Workers keep their own `Arc` clone, so the signal
/// survives the removal.
fn aio_mark_cancelled(key: i64) {
    if let Some(flag) = aio_cancel_flags().write().remove(&key) {
        flag.store(true, Ordering::SeqCst);
    }
}

/// How long a worker parks inside one poll before re-asking whether its channel
/// was closed under it. Same value and same role as
/// `net.rs::NET_READ_CLOSE_POLL_MS`.
const AIO_CLOSE_POLL_MS: i32 = 25;

/// The error a parked worker reports once its channel has been closed.
///
/// `ErrorKind::Interrupted` is the carrier every close-aware path in this tree
/// uses. The three read arms translate it into their existing EOF completion
/// rather than a new outcome type, because that is what the close already
/// produced on Linux via the `shutdown` above and what the `CompletionHandler`
/// contract expects for a channel that went away.
fn aio_closed_err() -> std::io::Error {
    std::io::Error::new(ErrorKind::Interrupted, "channel closed")
}

/// The deadline a close-aware worker loop must honour, or `None` for none.
///
/// `SO_RCVTIMEO`/`SO_SNDTIMEO` bound the *syscall*, and a syscall we no longer
/// issue until the socket is ready is one they can never bound. The timed
/// overload of `AsynchronousSocketChannel.read` sets a real read timeout on the
/// worker's private clone (`aio_asc_read_handler`), so the loop reads it back
/// and enforces it itself. Without this a wakeup fix would have removed one
/// hang and introduced another on every timed read.
fn aio_deadline(timeout: std::io::Result<Option<Duration>>) -> Option<std::time::Instant> {
    timeout
        .ok()
        .flatten()
        .map(|t| std::time::Instant::now() + t)
}

/// The poll slice for this pass: `AIO_CLOSE_POLL_MS`, clamped to the time left
/// before `deadline`. `Err(TimedOut)` once it has passed, so the wait actually
/// ends rather than merely declining to poll again.
fn aio_poll_slice(deadline: Option<std::time::Instant>) -> std::io::Result<i32> {
    match deadline {
        None => Ok(AIO_CLOSE_POLL_MS),
        Some(end) => {
            let left = end.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(std::io::Error::new(ErrorKind::TimedOut, "timed out"));
            }
            // Floor of 1 ms so a sub-millisecond remainder polls once more
            // rather than spinning on a 0 ms timeout.
            Ok(left.as_millis().clamp(1, AIO_CLOSE_POLL_MS as u128) as i32)
        }
    }
}

/// A blocking worker read that observes a close of the channel it is reading.
///
/// # On expiry
///
/// The per-pass `AIO_CLOSE_POLL_MS` slice expiring is not an outcome — it is
/// the point at which the cancellation flag is re-read, and the loop continues.
/// The socket's own read timeout, where one is set, IS an outcome and ends the
/// wait with `TimedOut`; see [`aio_deadline`].
fn aio_read_close_aware(
    stream: &TcpStream,
    buf: &mut [u8],
    cancel: &AtomicBool,
) -> std::io::Result<usize> {
    let deadline = aio_deadline(stream.read_timeout());
    loop {
        let slice = aio_poll_slice(deadline)?;
        let ready = match crate::net::poll_stream_readable(stream, slice) {
            Some(result) => result?,
            // No poll primitive on this target: the pre-2026-08-12 blocking
            // read, which sees only the `shutdown` wakeup.
            None => return crate::eintr::EintrIo::new(&mut &*stream).read(buf),
        };
        // Asked AFTER the poll so a close landing while we are parked is seen
        // on the very next pass.
        if cancel.load(Ordering::SeqCst) {
            return Err(aio_closed_err());
        }
        if !ready {
            continue;
        }
        return crate::eintr::EintrIo::new(&mut &*stream).read(buf);
    }
}

/// Largest payload handed to one `send` while a worker write is sliced. See
/// `socket_channel::WRITE_SLICE_MAX` for the measured reason a blocking write
/// has to be sliced at all.
const AIO_WRITE_SLICE_MAX: usize = 8 * 1024;

/// A blocking worker write that observes a close of the channel it is writing.
/// The write twin of [`aio_read_close_aware`]; returns the bytes transferred so
/// far when the close lands mid-write, which is what the existing partial-write
/// error path already reports to the handler.
fn aio_write_close_aware(
    stream: &TcpStream,
    data: &[u8],
    cancel: &AtomicBool,
) -> std::io::Result<usize> {
    let deadline = aio_deadline(stream.write_timeout());
    let mut written: usize = 0;
    loop {
        if written == data.len() {
            return Ok(written);
        }
        let slice = match aio_poll_slice(deadline) {
            Ok(slice) => slice,
            // A timeout after a partial transfer answers the partial count,
            // matching the pre-existing partial-write reporting on this path.
            Err(_) if written > 0 => return Ok(written),
            Err(e) => return Err(e),
        };
        let ready = match crate::net::poll_stream_writable(stream, slice) {
            Some(result) => result?,
            None => {
                let mut w = stream;
                w.write_all(&data[written..])?;
                return Ok(data.len());
            }
        };
        if cancel.load(Ordering::SeqCst) {
            if written > 0 {
                return Ok(written);
            }
            return Err(aio_closed_err());
        }
        if !ready {
            continue;
        }
        let end = (written + AIO_WRITE_SLICE_MAX).min(data.len());
        let mut w = stream;
        match w.write(&data[written..end]) {
            Ok(0) => return Err(std::io::Error::from(ErrorKind::WriteZero)),
            Ok(n) => written += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
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
    // 2026-08-12: the `shutdown` above is the wakeup on Linux and NOT on
    // Windows, where no `shutdown` aborts a pending blocking call. The flag is
    // what reaches a worker parked on a private `try_clone`d duplicate there.
    aio_mark_cancelled(aio_cancel_key_registry(id));
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
    // See `aio_shutdown_stream`: the shutdown is the Linux wakeup, the flag is
    // the one that reaches a worker parked on a duplicate handle on Windows.
    aio_mark_cancelled(aio_cancel_key_fd(fd));
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

/// The `CompletionHandler` + `attachment` pair a queued AIO op must keep
/// reachable until its completion is delivered, held as **global GC roots**.
///
/// AUDIT 2026-08-01 (native-collections-root-audit): these two used to travel
/// through `Job`/`Completion` as bare `ObjectRef`s. That is a scan/remap hole
/// of the worst kind on this path — the reference is parked on a worker thread
/// across a *blocking* syscall (`connect(2)` waits up to the 30 s policy
/// timeout, `accept(2)` waits indefinitely on an idle server) and then hops a
/// queue to the dispatcher. Nothing rooted it, so the collector was free to
/// reclaim the handler outright, and nothing remapped it, so a moving
/// collection left the `ObjectRef` pointing at whatever later occupied the
/// address. `invoke_virtual(handler, "completed", ...)` then dispatched on a
/// recycled object. The sibling `ReadCompletion`/`FutureCompletion` paths in
/// this same file already did this correctly (`handler_gref`/`attachment_gref`)
/// and `Job::Write` had been converted for its *buffer* alone in the 2026-07-26
/// audit — the handler on the very same struct was left bare.
///
/// `0` is the "absent" handle in both slots, matching `add_global_root`'s
/// no-op contract for mock contexts. Ownership rule: whoever holds a
/// `HandlerRoots` owns the roots. Exactly one of `push_handler_completion`
/// (which transfers ownership to the dispatcher) or [`queue_handler_release`]
/// (which parks them for release) must be reached on every path, or the roots
/// leak.
#[derive(Clone, Copy, Default)]
pub struct HandlerRoots {
    /// Global-root handle for the `CompletionHandler` (0 ⇒ no handler, i.e.
    /// nothing to deliver).
    pub handler: usize,
    /// Global-root handle for the `attachment` (0 ⇒ null attachment).
    pub attachment: usize,
}

impl HandlerRoots {
    /// Root a handler/attachment pair on the VM thread that is enqueuing the
    /// job. Must be called with a `&mut NativeContext`; worker threads have
    /// none, which is exactly why the roots have to be taken here.
    fn new(
        ctx: &mut dyn NativeContext,
        handler: Option<ObjectRef>,
        attachment: Option<ObjectRef>,
    ) -> Self {
        HandlerRoots {
            handler: handler.map(|h| ctx.add_global_root(h)).unwrap_or(0),
            attachment: attachment.map(|a| ctx.add_global_root(a)).unwrap_or(0),
        }
    }

    /// True when there is a handler to deliver to.
    fn has_handler(self) -> bool {
        self.handler != 0
    }
}

/// Park both roots of a `HandlerRoots` for release on the next user-thread
/// drain. Used by worker paths that decide there is nothing to deliver — a
/// job whose `CompletionHandler` was null still rooted its attachment.
fn queue_handler_release(roots: HandlerRoots) {
    queue_root_release(roots.handler);
    queue_root_release(roots.attachment);
}

/// A single completed AIO op waiting to be reported to its
/// `CompletionHandler` on a user-facing JVM thread.
pub struct Completion {
    /// Global roots for the CompletionHandler and its attachment. Owned by
    /// this completion; released by `drain_completions` after delivery.
    pub roots: HandlerRoots,
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
    // The flushes above are pure VM-state bookkeeping and are safe on any
    // thread. DISPATCHING is not: it runs arbitrary Java. Only a designated
    // completion thread may do that — see `on_completion_thread`. An
    // application thread that merely called some other AIO native (a
    // `write`, an `isOpen`) must not have a `CompletionHandler` run on it.
    if !on_completion_thread() && dispatcher_available() {
        if !completion_queue().lock().is_empty() {
            ensure_dispatcher();
            // Same condvar `push_handler_completion` signals.
            read_completion_state().1.notify_one();
        }
        return;
    }
    for _ in 0..DRAIN_LIMIT {
        let next = completion_queue().lock().pop_front();
        let Some(c) = next else { break };
        // AUDIT 2026-08-01: resolve the handler/attachment through their global
        // roots — the *current*, post-relocation addresses — instead of using
        // the raw `ObjectRef`s the worker captured before it blocked. The roots
        // are released once, below, on every exit from this iteration.
        let Some(handler) = ctx.resolve_global_root(c.roots.handler) else {
            // The handle is gone (or was never taken: a handler-less job must
            // not reach the completion queue at all). Nothing to deliver;
            // still drop whatever the attachment slot held.
            ctx.remove_global_root(c.roots.handler);
            ctx.remove_global_root(c.roots.attachment);
            continue;
        };
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
                        // Concrete, per `ASC_IMPLS`; see `aio_base`.
                        let minted = crate::concrete_receiver::alloc_concrete(
                            ctx,
                            ASC_IMPLS,
                            "java/nio/channels/AsynchronousSocketChannel",
                            N_FIELDS,
                        );
                        let ch = minted.obj;
                        aio_set(ctx, ch, F_OPEN, Value::Int(1));
                        aio_set(ctx, ch, F_CONNECTED, Value::Int(1));
                        aio_set(ctx, ch, F_REG_ID, Value::Int(new_id));
                        aio_set(ctx, ch, F_REMOTE, Value::Object(None));
                        Value::Object(Some(ch))
                    }
                };
                // `new_object` / `alloc_obj` above can allocate and therefore
                // move the handler and attachment. Re-read both through their
                // roots rather than reusing the pre-allocation locals.
                let handler = ctx.resolve_global_root(c.roots.handler).unwrap_or(handler);
                let attach = Value::Object(ctx.resolve_global_root(c.roots.attachment));
                let inv = ctx.invoke(
                    "java/nio/channels/CompletionHandler",
                    "completed",
                    "(Ljava/lang/Object;Ljava/lang/Object;)V",
                    &[Value::Object(Some(handler)), result_val, attach],
                );
                // A `CompletionHandler.completed` that throws used to be
                // discarded silently (`let _ = ctx.invoke(..)`), which hid
                // real failures in the accept path for a long time.
                if let Err(ref e) = inv {
                    dbg_aio!("DELIVER completed threw {:?}", e);
                }
            }
            Err(msg) => {
                // Allocate the message string FIRST: `create_string` is an
                // allocation, so building it after the exception would leave
                // `throw` — an unrooted local — pointing at a pre-move address
                // by the time `set_field_by_name` writes through it.
                let m = ctx.create_string(&msg);
                let m_gref = ctx.add_global_root(m);
                let throw = match ctx.new_object("java/io/IOException") {
                    Ok(Some(Value::Object(Some(t)))) => t,
                    _ => {
                        // Nothing can be delivered — release the roots rather
                        // than leaking them on the way out.
                        ctx.remove_global_root(m_gref);
                        ctx.remove_global_root(c.roots.handler);
                        ctx.remove_global_root(c.roots.attachment);
                        continue;
                    }
                };
                let m = ctx.resolve_global_root(m_gref).unwrap_or(m);
                ctx.remove_global_root(m_gref);
                ctx.set_field_by_name(throw, "detailMessage", Value::Object(Some(m)));
                // Same hazard as the `Ok` arm: `new_object` + `create_string`
                // are allocations, so refresh through the roots.
                let handler = ctx.resolve_global_root(c.roots.handler).unwrap_or(handler);
                let attach = Value::Object(ctx.resolve_global_root(c.roots.attachment));
                let _ = ctx.invoke(
                    "java/nio/channels/CompletionHandler",
                    "failed",
                    "(Ljava/lang/Throwable;Ljava/lang/Object;)V",
                    &[
                        Value::Object(Some(handler)),
                        Value::Object(Some(throw)),
                        attach,
                    ],
                );
            }
        }
        // Delivery is done (or was skipped): the completion owned these roots,
        // so drop them exactly once here.
        ctx.remove_global_root(c.roots.handler);
        ctx.remove_global_root(c.roots.attachment);
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
    // This IS the pool thread of our `AsynchronousChannelGroup` equivalent, so
    // it — and anything it calls, including a handler re-arming its next read —
    // may invoke Java completion callbacks.
    let _delivering = CompletionThreadGuard::enter();
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
                    // See `native_cf_complete`: `postComplete` is bytecode,
                    // so the by-NAME native resolver misses on every call.
                    let _ = ctx.invoke_virtual_bytecode_only(future, "postComplete", "()V", &[]);
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

thread_local! {
    /// True while this thread is acting as an AIO *completion-delivery* thread
    /// — i.e. it is the VM-attached dispatcher (`drain_completions_pub`), or
    /// the JDK's own port dispatcher loop (`Iocp`/`EPollPort`/`KQueuePort`
    /// `drain`/`poll`), or it is already inside a delivery on one of those.
    ///
    /// Invoking a Java `CompletionHandler` is only permitted on such a thread.
    /// `AsynchronousChannelGroup` allows an immediate completion to run on the
    /// initiating thread ONLY "where ... the initiating thread is one of the
    /// pooled threads in the group" — the qualifier exists precisely so an
    /// application thread is never hijacked into running a handler that may
    /// block. See [`on_completion_thread`].
    static DELIVERING_COMPLETIONS: Cell<bool> = const { Cell::new(false) };
}

/// Marks the calling thread as a completion-delivery thread for its lifetime,
/// restoring the previous state on drop (so nesting and unwinding are safe).
struct CompletionThreadGuard(bool);

impl CompletionThreadGuard {
    fn enter() -> Self {
        CompletionThreadGuard(DELIVERING_COMPLETIONS.with(|c| c.replace(true)))
    }
}

impl Drop for CompletionThreadGuard {
    fn drop(&mut self) {
        DELIVERING_COMPLETIONS.with(|c| c.set(self.0));
    }
}

/// Whether this thread may invoke Java `CompletionHandler` callbacks.
///
/// BUG (TestWsRemoteEndpointImplServerDeadlock, 2026-08-01): before this gate,
/// ANY thread that armed a handler-form read whose bytes had already arrived
/// ran `completed()` itself, and any thread that called an unrelated AIO native
/// drained the pending-handler queue. Tomcat's WebSocket client arms its first
/// read from `WsFrameClient.startInputProcessing`, which runs on the
/// APPLICATION thread inside `WsWebSocketContainer.connectToServer`. On
/// loopback the server's frames are already buffered, so `completed()` fired
/// inline, re-armed, completed inline again, and delivered a whole text message
/// to `onMessage` — all on the application thread, still inside
/// `connectToServer`. That test's `@OnMessage` blocks on a latch that only the
/// application thread can count down, so it deadlocked permanently (~35-50% of
/// runs, whenever the frames happened to be buffered in time).
///
/// A completion parked for the dispatcher is never delayed by this: every
/// `push_*_completion` signals the dispatcher's condvar.
fn on_completion_thread() -> bool {
    DELIVERING_COMPLETIONS.with(|c| c.get())
}

/// Whether a real dispatcher can be started. Pure-Rust users of this crate (the
/// in-crate tests) never install a launcher, and for them the opportunistic
/// drain remains the only delivery mechanism — so the gate above must not
/// strand completions there.
fn dispatcher_available() -> bool {
    dispatcher_launcher().lock().is_some()
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
        /// Global roots for the handler/attachment pair — see [`HandlerRoots`].
        /// `policy_connect` blocks for up to the configured connect timeout
        /// (30 s by default), so this is one of the longest GC windows in the
        /// crate.
        roots: HandlerRoots,
        /// Round-8 C29: global-root handle for the user-visible
        /// `AsynchronousSocketChannel` object whose `F_CONNECTED` flag was set
        /// optimistically by `aio_asc_connect` before this job ran. On connect
        /// failure the worker parks a `PendingFieldReset` so the next
        /// user-thread drain clears it back to 0 — otherwise the channel would
        /// lie to `isConnected()` after a failed connect.
        ///
        /// AUDIT 2026-08-01: was a bare `ObjectRef`. The reset is applied on a
        /// *different* thread after a blocking connect, so writing through the
        /// captured address could clear `F_CONNECTED` on an unrelated object.
        /// `0` ⇒ no channel to reset.
        channel_gref: usize,
    },
    // REMOVED 2026-08-01 (native-collections-root-audit): `Job::Read`, the
    // registry-backed handler-form read. It was already UNREACHABLE — nothing
    // in the crate constructed it, the handler-form read having been rerouted
    // to `Job::ReadFd`, which holds every Java reference as a global root.
    // What it left behind was a liability rather than dead weight: its
    // `bb_arr`/`bb_obj`/`handler`/`attachment` were bare `ObjectRef`s parked
    // across a *blocking* worker-thread read, scanned by nothing and remapped
    // by nothing, and it was the only consumer of the equally unrooted
    // `PendingArrayWrite` side table (removed with it). Anyone restoring a
    // registry-backed read must root its references the way `Job::ReadFd`
    // does; see `native-collections-root-audit.md`.
    Write {
        id: i32,
        data: Vec<u8>,
        /// Global-root handle for the source `ByteBuffer`, so the dispatcher
        /// can advance its `position` by the bytes actually written (see
        /// `CompletionKind::WriteCount`). This used to be a raw `ObjectRef`
        /// (`bb_obj`) held across a worker-thread blocking write — both
        /// unrooted against a moving GC and, worse, simply discarded.
        bb_gref: usize,
        /// AUDIT 2026-08-01: the 2026-07-26 audit rooted the buffer above and
        /// left the handler/attachment on this same struct bare. See
        /// [`HandlerRoots`].
        roots: HandlerRoots,
    },
    Accept {
        id: i32,
        /// AUDIT 2026-08-01: `accept(2)` blocks indefinitely on an idle
        /// listener, so an unrooted handler here was the longest-lived
        /// dangling `ObjectRef` in the crate. See [`HandlerRoots`].
        roots: HandlerRoots,
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
        /// Set by this channel's close so the worker's poll loop can end a park
        /// the `shutdown` cannot reach. See `aio_cancel_flag`.
        cancel: Arc<AtomicBool>,
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
        /// See `Job::ReadFd::cancel`.
        cancel: Arc<AtomicBool>,
        future_gref: usize,
        buffer_gref: usize,
    },
    /// Future-form write against an fd_table-backed channel.  This is the
    /// important counterpart to `ReadFutureFd`: the caller must receive a
    /// pending Future immediately rather than block in `send()`.
    WriteFutureFd {
        stream: Arc<Mutex<TcpStream>>,
        data: Vec<u8>,
        /// See `Job::ReadFd::cancel`.
        cancel: Arc<AtomicBool>,
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
            roots,
            channel_gref,
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
                    queue_root_release(channel_gref);
                    if roots.has_handler() {
                        push_handler_completion(Completion {
                            roots,
                            outcome: Ok(CompletionKind::Void),
                        });
                    } else {
                        queue_handler_release(roots);
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
                    if channel_gref != 0 {
                        // Both resets name the SAME root handle; the flusher
                        // releases it once, after the last entry (see
                        // `flush_pending_field_resets`).
                        let mut resets = pending_field_resets().lock();
                        resets.push(PendingFieldReset {
                            target_gref: channel_gref,
                            field: F_CONNECTED,
                            value: Value::Int(0),
                            release_root: false,
                        });
                        // Also clear the registry id so callers don't
                        // try to look up a now-removed handle.
                        resets.push(PendingFieldReset {
                            target_gref: channel_gref,
                            field: F_REG_ID,
                            value: Value::Int(-1),
                            release_root: true,
                        });
                    }
                    if roots.has_handler() {
                        push_handler_completion(Completion {
                            roots,
                            outcome: Err(format!("connect failed: {e}")),
                        });
                    } else {
                        queue_handler_release(roots);
                    }
                }
            }
        }
        Job::Write {
            id,
            data,
            bb_gref,
            roots,
        } => {
            // This arm carries the registry `id`, so its close question is the
            // ordinary one every other close-aware site asks. The flag is
            // obtained here rather than passed in because this job's queueing
            // side predates the plumbing; `aio_cancel_flag` is idempotent.
            let cancel = aio_cancel_flag(aio_cancel_key_registry(id));
            let stream = {
                let map = aio_registry().read();
                match map.get(&id) {
                    Some(AioHandle::Stream(s)) => Arc::clone(s),
                    _ => {
                        queue_root_release(bb_gref);
                        if roots.has_handler() {
                            push_handler_completion(Completion {
                                roots,
                                outcome: Err("write: channel closed".to_string()),
                            });
                        } else {
                            queue_handler_release(roots);
                        }
                        return Ok(());
                    }
                }
            };
            // ASYNCHRONOUS CLOSE 2026-08-12: the loop this replaces parked
            // inside a blocking `send` behind peer backpressure and observed
            // nothing when the channel was closed. `aio_write_close_aware`
            // polls for writability, re-reads the cancellation flag, and slices
            // -- the same loop as every other write in this family.
            let res = {
                let s = stream.lock();
                aio_write_close_aware(&s, &data, &cancel)
            };
            match res {
                Ok(n) => {
                    if roots.has_handler() {
                        // `WriteCount` carries the buffer root so the
                        // dispatcher advances `position` by `n` before
                        // invoking `completed()`; it also releases the root.
                        push_handler_completion(Completion {
                            roots,
                            outcome: Ok(CompletionKind::WriteCount {
                                n: n as i32,
                                buffer_gref: bb_gref,
                            }),
                        });
                    } else {
                        // No handler to deliver to — still advance the
                        // buffer (a write DID happen) and drop the roots.
                        queue_write_advance(bb_gref, n as i32);
                        queue_handler_release(roots);
                    }
                }
                Err(e) => {
                    // A partial write before the error still consumed
                    // `written` bytes from the socket's point of view, but the
                    // JDK reports the operation as failed and leaves the
                    // buffer position unspecified; just release the root.
                    queue_root_release(bb_gref);
                    if roots.has_handler() {
                        push_handler_completion(Completion {
                            roots,
                            outcome: Err(format!("write failed: {e}")),
                        });
                    } else {
                        queue_handler_release(roots);
                    }
                }
            }
        }
        Job::Accept { id, roots } => {
            let listener = {
                let map = aio_registry().read();
                match map.get(&id) {
                    Some(AioHandle::Listener(l, _)) => Arc::clone(l),
                    _ => {
                        if roots.has_handler() {
                            push_handler_completion(Completion {
                                roots,
                                outcome: Err("accept: channel closed".to_string()),
                            });
                        } else {
                            queue_handler_release(roots);
                        }
                        return Ok(());
                    }
                }
            };
            // ASYNCHRONOUS CLOSE 2026-08-12: a bare `accept()` here parked
            // until somebody connected, and closing the channel removed the
            // registry entry without ever waking the worker -- so the
            // `CompletionHandler` never fired and the pool thread was consumed
            // permanently. Same shape as `net::net_accept_close_aware`: flip
            // the listener non-blocking and re-ask the registry on every pass.
            // Safe to flip here, unlike on the shared surfaces, because this
            // listener is reachable only through `aio_registry` and only this
            // arm accepts on it.
            let res = {
                let l = listener.lock();
                if l.set_nonblocking(true).is_err() {
                    // No non-blocking mode on this handle: the pre-2026-08-12
                    // blocking accept, which cannot see the close.
                    crate::eintr::retry_eintr(|| l.accept())
                } else {
                    loop {
                        match l.accept() {
                            Ok(pair) => break Ok(pair),
                            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                                if !matches!(
                                    aio_registry().read().get(&id),
                                    Some(AioHandle::Listener(_, _))
                                ) {
                                    break Err(aio_closed_err());
                                }
                                std::thread::sleep(Duration::from_millis(AIO_CLOSE_POLL_MS as u64));
                            }
                            // EINTR on a parked accept is a transient
                            // interruption, not a failed accept -- see
                            // `crate::eintr`.
                            Err(e) if crate::eintr::is_eintr(&e) => continue,
                            Err(e) => break Err(e),
                        }
                    }
                }
            };
            match res {
                Ok((stream, _peer)) => {
                    let new_id = aio_register(AioHandle::Stream(Arc::new(Mutex::new(stream))));
                    if roots.has_handler() {
                        push_handler_completion(Completion {
                            roots,
                            outcome: Ok(CompletionKind::AcceptedChannel(new_id)),
                        });
                    } else {
                        queue_handler_release(roots);
                    }
                }
                Err(e) => {
                    if roots.has_handler() {
                        push_handler_completion(Completion {
                            roots,
                            outcome: Err(format!("accept failed: {e}")),
                        });
                    } else {
                        queue_handler_release(roots);
                    }
                }
            }
        }
        Job::ReadFd {
            stream,
            len,
            cancel,
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
                // `EintrIo` (inside `aio_read_close_aware`): this worker thread
                // is signalled like any other by the cross-thread JIT root
                // scan, and a bare EINTR here reaches the application's
                // `CompletionHandler.failed`.
                aio_read_close_aware(&s, &mut buf, &cancel)
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
            cancel,
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
                // Same as the handler-form arm above.
                aio_read_close_aware(&s, &mut buf, &cancel)
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
            cancel,
            future_gref,
            buffer_gref,
        } => {
            dbg_aio!(
                "WRITE worker start  future_gref={future_gref} data_len={} thread={:?}",
                data.len(),
                std::thread::current().id()
            );
            // Same close-aware write as the `Job::Write` arm above.
            let write_res = {
                let s = stream.lock();
                aio_write_close_aware(&s, &data, &cancel)
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

// REMOVED 2026-08-01 (native-collections-root-audit): `PendingArrayWrite` and
// `pending_array_writes`. The struct parked the destination `byte[]` of an
// async read as a bare `ObjectRef` on a worker thread and wrote into it later
// on a user thread — unrooted across a blocking `read(2)`, so the array could
// be reclaimed (the flush then read `array_length == 0` and silently dropped
// the bytes the application had asked for) or relocated (the bulk write landed
// in whatever now occupied the address). It was reachable only from the
// already-unreachable `Job::Read`, so removing that arm removed the only
// producer. `flush_pending_array_writes_inner` is kept as a no-op shim below
// so the four drain sites keep their shape and a future rooted implementation
// has an obvious home.

/// No-op since the unrooted `PendingArrayWrite` park was removed; retained so
/// the drain sites keep documenting where a rooted heap-buffer flush would go.
fn flush_pending_array_writes_inner(_ctx: &mut dyn NativeContext) {}

/// Round-8 C29 fix: workers can't touch the user-visible Java object
/// directly (no `&mut NativeContext`). When a worker needs to reset a
/// field on a channel — e.g. clearing `F_CONNECTED = 0` after a connect
/// failure — it parks a `PendingFieldReset` here and the next AIO native
/// call on the user thread drains it via `flush_pending_field_resets`.
/// AUDIT 2026-08-01: `target` was a bare `ObjectRef`. The park happens on a
/// worker thread after a failed `connect(2)` and the apply happens later on a
/// user thread, so a moving collection in between repointed the channel and
/// this wrote `F_CONNECTED = 0` into whatever now lived at the old address —
/// or, if the channel had died, into reclaimed memory. It is a global root
/// handle now; `release_root` marks the LAST entry for a given handle so the
/// flusher drops the root exactly once (the connect-failure path parks two
/// resets against one channel).
struct PendingFieldReset {
    target_gref: usize,
    field: usize,
    value: Value,
    release_root: bool,
}

fn pending_field_resets() -> &'static Mutex<Vec<PendingFieldReset>> {
    static V: OnceLock<Mutex<Vec<PendingFieldReset>>> = OnceLock::new();
    V.get_or_init(|| Mutex::new(Vec::new()))
}

fn flush_pending_field_resets(ctx: &mut dyn NativeContext) {
    let parked = std::mem::take(&mut *pending_field_resets().lock());
    for r in parked {
        // Resolve through the root: this is the target's CURRENT address, not
        // the one the worker captured before it blocked.
        if let Some(target) = ctx.resolve_global_root(r.target_gref) {
            // `r.field` is a LOGICAL index into this module's private map, not
            // an absolute slot: the worker that parked it cannot know the
            // receiver's class, and since 2026-08-21 the map is appended above
            // whatever the concrete `sun.nio.ch.*Impl` declares. Resolve the
            // base here, where `ctx` is available.
            if aio_has(ctx, target, r.field) {
                aio_set(ctx, target, r.field, r.value);
            }
        }
        if r.release_root {
            ctx.remove_global_root(r.target_gref);
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
                ctx.set_field_by_name(
                    bb,
                    "position",
                    Value::Int(position.saturating_add(r.advance)),
                );
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

/// The concrete `AsynchronousSocketChannel` HotSpot 25 builds, per platform.
/// Ordered; only one is present in any one image.
///
/// `pub` because `native-builtins/src/phases_late/net_channels.rs` owns the one
/// `connect(Ljava/net/SocketAddress;)Ljava/util/concurrent/Future;` registration
/// in the tree and has to put it on these classes too.
pub const ASC_IMPLS: &[&str] = &[
    "sun/nio/ch/UnixAsynchronousSocketChannelImpl",
    "sun/nio/ch/WindowsAsynchronousSocketChannelImpl",
];
/// Its abstract parent, which declares most of the family with `Code`. It is
/// a MIRROR target, never a mint target -- it is abstract, which is the whole
/// defect.
pub const ASC_ABSTRACT_IMPL: &str = "sun/nio/ch/AsynchronousSocketChannelImpl";

/// The concrete `AsynchronousServerSocketChannel`, per platform.
const ASSC_IMPLS: &[&str] = &[
    "sun/nio/ch/UnixAsynchronousServerSocketChannelImpl",
    "sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl",
];
const ASSC_ABSTRACT_IMPL: &str = "sun/nio/ch/AsynchronousServerSocketChannelImpl";

/// The concrete `AsynchronousChannelGroup`: the platform's completion port.
/// `AsynchronousChannelGroup.withFixedThreadPool` on Linux answers a
/// `sun.nio.ch.EPollPort` (MEASURED, `probes/W4Abstract.java` oracle column).
const ACG_IMPLS: &[&str] = &[
    "sun/nio/ch/EPollPort",
    "sun/nio/ch/Iocp",
    "sun/nio/ch/KQueuePort",
    "sun/nio/ch/SolarisEventPort",
];
/// Its two abstract parents, mirror targets for the same reason as
/// [`ASC_ABSTRACT_IMPL`]: `shutdown`/`isShutdown`/`awaitTermination` are
/// declared there, with `Code`.
const ACG_ABSTRACT_IMPLS: &[&str] = &["sun/nio/ch/Port", "sun/nio/ch/AsynchronousChannelGroupImpl"];

/// Where this module's private slot map (`F_OPEN`..`F_REMOTE`) starts on `o`.
///
/// **This is the repair `aio_assc_open`'s doc comment asked for and could not
/// make.** Until 2026-08-21 the four constants were ABSOLUTE indices, and the
/// three factories minted objects whose class NAME was the abstract public API
/// class -- so `F_OPEN` wrote an `Int` into the slot the real
/// `java.nio.channels.AsynchronousServerSocketChannel` layout calls `provider`
/// (a reference the collector scans as an oop) and the other three sat past the
/// end of everything the class declared. Both halves are fixed together, which
/// is what that comment said the repair required:
///
///   * the mints now name the CONCRETE `sun.nio.ch.*Impl` class (JVMS 6.5 -- a
///     receiver `new` cannot produce is a defect with no oracle run required,
///     `H21-1` N3), and
///   * the private map is APPENDED above whatever that class declares, through
///     this one function, called the same way by every allocator and every
///     accessor so the two can never disagree.
///
/// The width guard inside `concrete_base` collapses the base to 0 for any
/// receiver this module did not allocate, so the shared `aio_asc_is_open` still
/// answers correctly for a foreign or stub-mode object -- which is the other
/// thing that comment said a one-sided renumber would break.
fn aio_base(ctx: &mut dyn NativeContext, o: ObjectRef) -> usize {
    crate::concrete_receiver::concrete_base(ctx, o, N_FIELDS)
}

/// Read private slot `idx` of an async channel.
fn aio_get(ctx: &mut dyn NativeContext, o: ObjectRef, idx: usize) -> Value {
    let base = aio_base(ctx, o);
    ctx.get_field(o, base + idx)
}

/// Write private slot `idx` of an async channel.
fn aio_set(ctx: &mut dyn NativeContext, o: ObjectRef, idx: usize, v: Value) {
    let base = aio_base(ctx, o);
    ctx.set_field(o, base + idx, v);
}

/// Whether `o` is wide enough to carry private slot `idx`.
fn aio_has(ctx: &mut dyn NativeContext, o: ObjectRef, idx: usize) -> bool {
    let base = aio_base(ctx, o);
    ctx.object_num_fields(o) > base + idx
}

/// Record a completed blocking connect on a channel THIS module allocated.
///
/// Exported because `native-builtins/src/phases_late/net_channels.rs` owns the
/// one surviving `connect(Ljava/net/SocketAddress;)Ljava/util/concurrent/Future;`
/// registration on this class and used to write slots 0/2/3 by index under a
/// map whose slots 0 and 1 mean the OPPOSITE of this one's -- the
/// two-layouts-on-one-class condition W7-49 measured and could not repair from
/// one side. One writer, one map: that crate now calls this.
pub fn async_socket_note_connected(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    fd_id: i32,
    remote: Value,
) {
    aio_set(ctx, this, F_CONNECTED, Value::Int(1));
    aio_set(ctx, this, F_REG_ID, Value::Int(fd_id));
    aio_set(ctx, this, F_REMOTE, remote);
}

fn read_aio_id(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<i32> {
    if !aio_has(ctx, this, F_REG_ID) {
        return None;
    }
    match aio_get(ctx, this, F_REG_ID) {
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

/// The `AsynchronousChannelGroup`'s one private slot: its `group_registry` id,
/// appended above the concrete port class's own layout. Same rule as
/// [`aio_base`], with a width of one.
fn acg_group_base(ctx: &mut dyn NativeContext, o: ObjectRef) -> usize {
    crate::concrete_receiver::concrete_base(ctx, o, 1)
}

fn acg_has_id(ctx: &mut dyn NativeContext, o: ObjectRef) -> bool {
    let base = acg_group_base(ctx, o);
    ctx.object_num_fields(o) >= base + 1
}

fn acg_read_id(ctx: &mut dyn NativeContext, o: ObjectRef) -> Value {
    let base = acg_group_base(ctx, o);
    ctx.get_field(o, base)
}

fn aio_acg_with_fixed(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // `java.nio.channels.AsynchronousChannelGroup` is ABSTRACT; the group the
    // JDK hands back is the platform completion port (`ACG_IMPLS`).
    let minted = crate::concrete_receiver::alloc_concrete(
        ctx,
        ACG_IMPLS,
        "java/nio/channels/AsynchronousChannelGroup",
        1,
    );
    let (group, acg_base) = (minted.obj, minted.base);
    let id = group_next_id();
    group_registry().write().insert(
        id,
        GroupState {
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_ops: Arc::new(AtomicUsize::new(0)),
        },
    );
    ctx.set_field(group, acg_base, Value::Int(id));
    Ok(Some(Value::Object(Some(group))))
}

fn aio_acg_with_pool(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    aio_acg_with_fixed(ctx, args)
}

fn aio_acg_is_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = match obj_or_none(args, 0) {
        Some(o) if acg_has_id(ctx, o) => match acg_read_id(ctx, o) {
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
        Some(o) if acg_has_id(ctx, o) => match acg_read_id(ctx, o) {
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
        Some(o) if acg_has_id(ctx, o) => match acg_read_id(ctx, o) {
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
    // Concrete, per `ASC_IMPLS`; see `aio_base` for the two halves of the fix.
    let ch = crate::concrete_receiver::alloc_concrete(
        ctx,
        ASC_IMPLS,
        "java/nio/channels/AsynchronousSocketChannel",
        N_FIELDS,
    )
    .obj;
    aio_set(ctx, ch, F_OPEN, Value::Int(1));
    aio_set(ctx, ch, F_CONNECTED, Value::Int(0));
    aio_set(ctx, ch, F_REG_ID, Value::Int(-1));
    aio_set(ctx, ch, F_REMOTE, Value::Object(None));
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
        Some(o) if aio_has(ctx, o, F_OPEN) => Ok(Some(aio_get(ctx, o, F_OPEN))),
        _ => Ok(Some(Value::Int(0))),
    }
}

fn aio_asc_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(this) = obj_or_none(args, 0) {
        if dbg_aio_enabled() {
            let raw_id = if aio_has(ctx, this, F_REG_ID) {
                match aio_get(ctx, this, F_REG_ID) {
                    Value::Int(v) => v,
                    _ => i32::MIN,
                }
            } else {
                i32::MIN
            };
            dbg_aio!("CLOSE entered, raw F_REG_ID field={raw_id}");
        }
        if aio_has(ctx, this, F_OPEN) {
            aio_set(ctx, this, F_OPEN, Value::Int(0));
        }
        if aio_has(ctx, this, F_CONNECTED) {
            aio_set(ctx, this, F_CONNECTED, Value::Int(0));
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
            aio_set(ctx, this, F_REG_ID, Value::Int(-1));
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

    // AUDIT 2026-08-01: root the handler, attachment and channel HERE, on the
    // VM thread that still has a `NativeContext`. Two hazards, one fix:
    //   * `decode_addr` dispatches `getPort()`/`getHostString()` on the
    //     SocketAddress — real bytecode that can allocate and relocate all
    //     three of `this`/`handler`/`attachment` before they are ever used.
    //   * the worker then blocks in `policy_connect` for up to the connect
    //     timeout (30 s by default) holding them, with nothing scanning or
    //     remapping them for that whole window.
    let roots = HandlerRoots::new(ctx, handler, attachment);
    let channel_gref = ctx.add_global_root(this);
    let addr = match decode_addr(ctx, sa) {
        Ok(a) => a,
        Err(e) => {
            ctx.remove_global_root(channel_gref);
            ctx.remove_global_root(roots.handler);
            ctx.remove_global_root(roots.attachment);
            return Err(e);
        }
    };
    let this = ctx.resolve_global_root(channel_gref).unwrap_or(this);
    // Reserve an id up front in `Pending` state so close() can find it.
    let id = aio_register(AioHandle::Pending);
    let this = if ctx.object_num_fields(this) >= N_FIELDS {
        aio_set(ctx, this, F_REG_ID, Value::Int(id));
        let host_str = ctx.create_string(&addr);
        // `create_string` allocates: re-read the channel through its root
        // before writing the second field.
        let this = ctx.resolve_global_root(channel_gref).unwrap_or(this);
        aio_set(ctx, this, F_REMOTE, Value::Object(Some(host_str)));
        this
    } else {
        this
    };
    if let Err(e) = job_sender().send(Job::Connect {
        id,
        addr,
        roots,
        // Round-8 C29: pass the user-visible channel so the worker can
        // park a `F_CONNECTED = 0` reset on connect failure.
        channel_gref,
    }) {
        // The job never reached a worker, so nothing downstream will release
        // these — drop them here or they leak for the life of the VM.
        ctx.remove_global_root(channel_gref);
        ctx.remove_global_root(roots.handler);
        ctx.remove_global_root(roots.attachment);
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
    if aio_has(ctx, this, F_CONNECTED) {
        aio_set(ctx, this, F_CONNECTED, Value::Int(1));
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
    // HARD PRECONDITION, checked before anything else: only a designated
    // completion thread may run `completed()`. On any other thread this must
    // fall back to the worker pool no matter how ready the socket is — see
    // `on_completion_thread` for the deadlock this closes. (Checked first
    // because it is a TLS read, versus a `FIONREAD` syscall below.)
    if !on_completion_thread() {
        if aio_inline_dbg_enabled() {
            aio_inline_record(&AIO_INLINE_FOREIGN_THREAD);
        }
        return false;
    }
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
    let fd = match aio_get(ctx, this, F_REG_ID) {
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
            cancel: aio_cancel_flag(aio_cancel_key_for_reg_id(fd)),
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
/// Reads declined by the inline fast path because the initiating thread is not
/// a completion-delivery thread. Expected to be non-zero and harmless: it
/// counts the first read of each handler chain, which an application thread
/// arms (e.g. `WsFrameClient.startInputProcessing`). Only the follow-on reads,
/// armed from inside `completed()` on the dispatcher, are eligible to inline —
/// which is the case the fast path was built for.
static AIO_INLINE_FOREIGN_THREAD: AtomicUsize = AtomicUsize::new(0);

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

fn aio_latency_record(total: &AtomicUsize, count: &AtomicUsize, d: std::time::Duration) {
    total.fetch_add(
        d.as_nanos().min(usize::MAX as u128) as usize,
        Ordering::Relaxed,
    );
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
    let foreign = AIO_INLINE_FOREIGN_THREAD.load(Ordering::Relaxed);
    let total = taken + not_ready + capped + foreign;
    if total % 500 == 0 {
        eprintln!(
            "[DBG_AIO_INLINE] reads={total} inline={taken} not_ready={not_ready} \
             depth_capped={capped} foreign_thread={foreign} inline_rate={:.3}",
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
    let fd = match aio_get(ctx, this, F_REG_ID) {
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
            cancel: aio_cancel_flag(aio_cancel_key_for_reg_id(fd)),
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
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let bb = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(ioex("read: null ByteBuffer")),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let bb_pin = ctx.pin_native_root(bb);
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
    let this = ctx.read_native_pin(this_pin, this);
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

    // `F_REG_ID` is either an fd_table fd (channel produced by the Future-form
    // `connect` — the client path this native was originally written for) or,
    // for a channel handed to `AsynchronousServerSocketChannel.accept`'s
    // `CompletionHandler`, an `aio_registry` id >= `AIO_REG_BASE`. Only the
    // former used to be accepted, so EVERY server-side read failed with
    // "read: not connected" and no NIO2 server could serve a byte
    // (`TomcatServletWebServerFactoryTests.sslWithHttp11Nio2Protocol`).
    //
    // READ IT THROUGH `aio_get`, NOT AS A RAW SLOT. This was the one place in
    // this file that indexed a private slot directly, and on a REAL JDK channel
    // object it does not address `F_REG_ID` at all: `aio_base` exists precisely
    // because a concrete receiver carries the JDK's own fields first and this
    // module's private slots after them, so raw index 2 lands on an unrelated
    // real field. A synthetic 3-field carrier has `base == 0`, which is why this
    // worked everywhere it was tested and failed on the one path that gets a
    // concrete channel.
    //
    // MEASURED: `CRATONVM_DBG_AIO=1` on
    // `TestWsWebSocketContainerSessionExpirySession` shows the Future-form
    // read/write resolving `fd=9` and succeeding, and the handler-form read on
    // the SAME channel logging `this_fields=52` — a real
    // `AsynchronousSocketChannel` implementation object, not a 3-field carrier —
    // then failing `read: bad fd for tcp clone`. Tomcat's `WsFrameClient`
    // treats that failure as a dropped connection and closes the session
    // immediately after `onOpen`, which is the whole 19-class WebSocket cluster:
    // sessions are unregistered as fast as they are registered, so
    // `getOpenSessions()` never returns more than the caller.
    let slot2 = match aio_get(ctx, this, F_REG_ID) {
        Value::Int(v) if v >= 0 => v,
        _ => {
            post_immediate(ctx, ReadOutcome::Error("read: not connected".to_string()));
            return Ok(Some(Value::Object(None)));
        }
    };
    let bb = ctx.read_native_pin(bb_pin, bb);

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
            cancel: aio_cancel_flag(aio_cancel_key_for_reg_id(slot2)),
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
    // AUDIT 2026-08-01: root the handler/attachment alongside the buffer, and
    // do it BEFORE `read_buffer_bytes` — that walks the buffer's backing store
    // through the context and is a GC point. The 2026-07-26 audit rooted only
    // the buffer, and only after the read; the handler on the same job rode
    // across `write(2)` as a bare `ObjectRef`.
    //
    // The source buffer is rooted for the duration of the worker write: the
    // dispatcher must advance its `position` by the bytes actually written
    // (`AsynchronousByteChannel.write` contract), and a moving collection can
    // run while the worker is parked in `write(2)`.
    let roots = HandlerRoots::new(ctx, handler, attachment);
    let bb_gref = ctx.add_global_root(bb);
    let bb = ctx.resolve_global_root(bb_gref).unwrap_or(bb);
    let data = read_buffer_bytes(ctx, bb);
    if data.is_empty() {
        ctx.remove_global_root(bb_gref);
        if roots.has_handler() {
            push_handler_completion(Completion {
                roots,
                outcome: Ok(CompletionKind::IntCount(0)),
            });
        } else {
            queue_handler_release(roots);
        }
        return Ok(Some(Value::Object(None)));
    }
    if let Err(e) = job_sender().send(Job::Write {
        id,
        data,
        bb_gref,
        roots,
    }) {
        ctx.remove_global_root(bb_gref);
        ctx.remove_global_root(roots.handler);
        ctx.remove_global_root(roots.attachment);
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

/// **RESOLVED 2026-09-04 — this comment described a carrier this file no longer
/// mints, and its advice would now steer a reader away from a repair that has
/// already happened. The paragraphs below are kept because their reasoning is
/// still the right reasoning; read this block first.**
///
/// Everything after this point argues from a receiver whose class NAME is the
/// ABSTRACT `java.nio.channels.AsynchronousServerSocketChannel`, which declares
/// exactly one instance field (`provider`) — so `F_OPEN` landed in that slot and
/// the other three sat past the end of everything the class declares. That was
/// true when it was written. It is not true now: `alloc_concrete` mints the
/// CONCRETE impl, and both channels are byte-identical to HotSpot on the class
/// name (MEASURED, `probes/AioClassProbe`):
///
/// ```text
///                                    HotSpot 25                                CratonVM
/// AsynchronousServerSocketChannel.open()  sun.nio.ch.UnixAsynchronousServerSocketChannelImpl  same
/// AsynchronousSocketChannel.open()        sun.nio.ch.UnixAsynchronousSocketChannelImpl        same
/// ```
///
/// The concrete impls declare 16 and 48 fields, so the four VM fields are now
/// APPENDED past the real layout rather than overwriting slot 0 — which is
/// exactly the "appended-slot idiom" the paragraph below says cannot be applied
/// here. MEASURED with `CRATONVM_DBG_LAYOUT_ALIAS=1`:
///
/// ```text
/// UnixAsynchronousServerSocketChannelImpl   requested 20   real 16   over  +4
/// UnixAsynchronousSocketChannelImpl         requested 52   real 48   over  +4
/// ```
///
/// `+4` is the four constants, and nothing shares a slot with `provider` any
/// more. The residual `over` rows are the intended shape of appending, not a
/// collision. `provider()` itself is separately registered on both classes (it
/// answers the platform provider rather than reading the field at all), so the
/// symptom this comment predicted — *"`provider()` … would return that Int"* —
/// cannot occur either.
///
/// **What is still true:** the four constants are module-level and shared with
/// `AsynchronousSocketChannel`, and three registrations bind the same native to
/// both classes. Splitting the maps per class is still the tidier shape. It is
/// no longer a CORRECTNESS repair, so the warning below that renumbering "is out
/// of bounds" should not stop anyone — there is nothing broken left to break.
///
/// ---
///
/// LIVE 4-vs-1 over-allocation, MEASURED and deliberately NOT repaired.
/// W7-66-live-over-allocations.md.
///
/// `javap -p java.nio.channels.AsynchronousServerSocketChannel` on JDK
/// 25.0.3.9 declares exactly one instance field —
/// `private final AsynchronousChannelProvider provider` — on a class whose
/// superclass is `java.lang.Object`. So `F_OPEN` writes an `Int` into the slot
/// the real layout calls `provider` (the §5 shape of
/// natives-over-real-jdk-classes.md: a native's Int landing in a reference the
/// class declares), and `F_CONNECTED`/`F_REG_ID`/`F_REMOTE` sit past the end of
/// everything it declares. `provider()` is a real `final` accessor and would
/// return that Int.
///
/// The repair is the appended-slot idiom, and it cannot be applied to this
/// class alone. The four constants are module-level and shared with
/// `AsynchronousSocketChannel`, and three registrations bind the SAME native to
/// both classes — `isOpen` is `aio_asc_is_open`, which reads `F_OPEN` off
/// whichever receiver it gets. Renumbering here without renumbering there
/// breaks `isOpen` on every server channel; renumbering there is out of bounds,
/// because `AsynchronousSocketChannel` is the two-crates-one-class case W7-49
/// §5 measured: `native-builtins`' surviving `connect` triple reads slots 0..3
/// of objects THIS file allocates, under a map whose slots 0 and 1 mean the
/// opposite. A prior lane converted that side and correctly reverted it — a
/// repair to dead code that breaks the one live path is worse than none, and
/// the same holds for a repair here that breaks a shared native.
///
/// What this needs, and what this lane could not do: split the two slot maps
/// (a private one per class), give `aio_asc_is_open` a per-class sibling, and
/// settle the `native-builtins` survivor in the same step — a build, and one
/// change spanning both crates.
///
/// **THIS IS A FABRICATION SITE, and it is the one `H5-1` N7 could not find.**
/// The `alloc_obj` below mints an object whose class NAME is the abstract
/// `java.nio.channels.AsynchronousServerSocketChannel`. `H5-1` §3.2 listed 13
/// such sites and marked this class "abstract; no fabrication site found —
/// candidate, unproven", i.e. possibly movable down to a `sun.nio.ch.*Impl`.
/// It is not movable. The census missed it only because the call is split over
/// four lines and the grep matched `alloc_obj(ctx, "…"` on one — `[window≠absence]`.
///
/// MEASURED 2026-08-20 (H11), `fe59bf9d9`, `--jdk-only`:
/// `AsynchronousServerSocketChannel.open().getClass().getName()` answers
/// `java.nio.channels.AsynchronousServerSocketChannel`, where HotSpot 25.0.3+9
/// answers `sun.nio.ch.WindowsAsynchronousServerSocketChannelImpl`. Since
/// dispatch keys on the receiver's class, moving this class's 12 registrations
/// onto the `Impl` strands every receiver this function returns.
/// `AsynchronousSocketChannel.provider()` / `AsynchronousServerSocketChannel
/// .provider()` — the platform provider, not `null`.
///
/// Both are `final` accessors over a `private final AsynchronousChannelProvider
/// provider` field, and this file's own slot map parks `F_OPEN` in that slot
/// (see `aio_assc_open`'s doc comment, which measures the collision and
/// explains why renumbering it needs one change across two crates). The
/// accessor therefore could not read a provider out of the field, and answered
/// `null`.
///
/// Registering the accessor sidesteps the field entirely: the STATIC
/// `AsynchronousChannelProvider.provider()` already resolves on this VM —
/// MEASURED, it answers `sun.nio.ch.LinuxAsynchronousChannelProvider` — and it
/// is the same singleton HotSpot hands back from the instance accessor. So
/// this is a delegation to a working path, not a second source of truth, and
/// it leaves the slot-map repair exactly as open as it was.
///
/// MEASURED on HotSpot 25 (`probes/ResidualProbe.java`): the static and both
/// instance accessors return the SAME object.
fn aio_channel_provider(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    ctx.invoke(
        "java/nio/channels/spi/AsynchronousChannelProvider",
        "provider",
        "()Ljava/nio/channels/spi/AsynchronousChannelProvider;",
        &[],
    )
}

fn aio_assc_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let _ = job_sender();
    // Concrete, per `ASSC_IMPLS`. This is the site the doc comment above calls
    // "THIS IS A FABRICATION SITE" -- it is one no longer.
    let ch = crate::concrete_receiver::alloc_concrete(
        ctx,
        ASSC_IMPLS,
        "java/nio/channels/AsynchronousServerSocketChannel",
        N_FIELDS,
    )
    .obj;
    aio_set(ctx, ch, F_OPEN, Value::Int(1));
    aio_set(ctx, ch, F_CONNECTED, Value::Int(0));
    aio_set(ctx, ch, F_REG_ID, Value::Int(-1));
    aio_set(ctx, ch, F_REMOTE, Value::Object(None));
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
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
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
        let this = ctx.read_native_pin(this_pin, this);
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
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
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
    let this = ctx.read_native_pin(this_pin, this);
    if aio_has(ctx, this, F_REG_ID) {
        aio_set(ctx, this, F_REG_ID, Value::Int(id));
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
    // AUDIT 2026-08-01: an armed `accept()` blocks until a client shows up —
    // on an idle server, indefinitely. The handler and attachment used to ride
    // that entire wait as bare `ObjectRef`s: nothing kept them alive and
    // nothing repointed them, so the first connection of the day dispatched
    // `completed()` on whatever had since been allocated at those addresses.
    let roots = HandlerRoots::new(ctx, handler, attachment);
    if let Err(e) = job_sender().send(Job::Accept { id, roots }) {
        ctx.remove_global_root(roots.handler);
        ctx.remove_global_root(roots.attachment);
        eprintln!(
            "native-io: aio_assc_accept: job channel closed; \
             CompletionHandler will not fire (id={id}, err={e})"
        );
        return Err(ioex("accept: aio worker pool unavailable"));
    }
    Ok(Some(Value::Object(None)))
}

/// `AsynchronousServerSocketChannel.accept()` — the no-handler, `Future`-
/// returning overload. **A LOUD REFUSAL, deliberately, and it replaces a
/// louder-but-wrong one.**
///
/// # Why there is a native here at all
///
/// Until 2026-08-21 `aio_assc_open` minted an instance of the ABSTRACT
/// `java.nio.channels.AsynchronousServerSocketChannel`, on which this overload
/// is declared with no `Code`, so the call raised `AbstractMethodError` — a
/// refusal, by accident of the fabricated receiver. The receiver is now the
/// concrete `sun.nio.ch.UnixAsynchronousServerSocketChannelImpl` (JVMS 6.5 —
/// see `ASSC_IMPLS`), whose real `accept()` bytecode runs
/// `AsynchronousServerSocketChannelImpl.accept()` -> `implAccept()` and reads
/// the `localAddress` field that `aio_assc_bind` never wrote. MEASURED:
/// `RJdkAsyncChannel.acceptFutureMustNotHang` came back with
/// `NotYetBoundException` on a channel that had been bound — a WRONG answer
/// about the channel's state, where the old one was at least a true "not
/// implemented".
///
/// # Why a refusal rather than an implementation
///
/// The `Future` form has to complete LATER, from the accept worker, and this
/// module's completion path (`push_handler_completion` + `drain_completions`)
/// applies on the calling thread. A real `CompletableFuture` handed back here
/// would be completed only by a `drain` that a caller blocked in
/// `future.get(timeout)` never reaches — the hang that
/// `acceptFutureMustNotHang` exists to forbid, and the worst of the three
/// available answers. `accept(Object, CompletionHandler)` is the form this
/// module implements for real, and it is the one an idiomatic NIO2 server uses.
///
/// `UnsupportedOperationException` rather than `AbstractMethodError` because it
/// is TRUE: the method exists and this VM does not implement it. Both are
/// caught by any caller written to tolerate a partial NIO2, and the vector
/// accepts either.
///
/// NOMINATION: implementing this properly needs a completion path that can run
/// off the calling thread — the same thing `AsynchronousChannelGroup`'s own
/// dispatcher would need. Until then this refusal is the honest answer.
fn aio_assc_accept_future(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let _ = ctx;
    Err(RuntimeError::UnsupportedOperationException {
        message: "AsynchronousServerSocketChannel.accept() (the Future form) is not implemented \
                  by CratonVM; use accept(Object, CompletionHandler)"
            .to_string(),
    }
    .into())
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
    // `Iocp`/`EPollPort`/`KQueuePort` `drain`/`poll` are called from the JDK's
    // own dispatcher loop, so this caller is a pool thread by construction.
    let _delivering = CompletionThreadGuard::enter();
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
    // Where this registrar's rows start; the mirrors at its foot must not be
    // able to see another crate's. `mirror_class_registrations` says why.
    let __rows_before = r.dump_registrations().len();
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
    r.register(
        asc,
        "provider",
        "()Ljava/nio/channels/spi/AsynchronousChannelProvider;",
        aio_channel_provider,
    );
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
    r.register(
        asc,
        "supportedOptions",
        "()Ljava/util/Set;",
        aio_supported_options,
    );
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
    r.register(
        assc,
        "provider",
        "()Ljava/nio/channels/spi/AsynchronousChannelProvider;",
        aio_channel_provider,
    );
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
    r.register(
        assc,
        "supportedOptions",
        "()Ljava/util/Set;",
        aio_supported_options,
    );
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
    // The `Future`-returning overload. See `aio_assc_accept_future` for why it
    // is a refusal, and why a refusal had to be registered rather than left to
    // the JDK's own body once the receiver became concrete.
    r.register(
        assc,
        "accept",
        "()Ljava/util/concurrent/Future;",
        aio_assc_accept_future,
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

    // The registration half of the three fabricated-receiver fixes above.
    //
    // Dispatch keys on the receiver's runtime class (`H11-1`), and each of
    // these concrete classes -- and the abstract `sun.nio.ch.*Impl` it inherits
    // from -- declares this family with `Code`. Without the mirror, moving the
    // mint would hand every call to the JDK's own bodies, running against a
    // channel whose `<init>` this VM never ran.
    for target in ASC_IMPLS.iter().chain([ASC_ABSTRACT_IMPL].iter()) {
        crate::concrete_receiver::mirror_class_registrations(r, __rows_before, asc, target);
    }
    for target in ASSC_IMPLS.iter().chain([ASSC_ABSTRACT_IMPL].iter()) {
        crate::concrete_receiver::mirror_class_registrations(r, __rows_before, assc, target);
    }
    for target in ACG_IMPLS.iter().chain(ACG_ABSTRACT_IMPLS.iter()) {
        crate::concrete_receiver::mirror_class_registrations(r, __rows_before, acg, target);
    }

    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{confine_test_lock, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// No private slot in this module may be indexed RAW — every access goes
    /// through `aio_get`/`aio_set`/`aio_has`, which add `aio_base`.
    ///
    /// This is a source guard rather than a behavioural one because the defect
    /// it catches is invisible on the receiver shape the unit tests build. A
    /// synthetic 3-field carrier has `base == 0`, so `ctx.get_field(this, 2)`
    /// and `aio_get(ctx, this, F_REG_ID)` are the SAME slot and every test
    /// passes either way. They diverge only on a concrete receiver — a real JDK
    /// `AsynchronousSocketChannel` implementation object, which carries the
    /// JDK's own fields first and this module's private slots after them.
    ///
    /// That is not hypothetical. `aio_asc_read` indexed slot 2 directly, and on
    /// a concrete channel (`CRATONVM_DBG_AIO=1` reports `this_fields=52`) it
    /// read an unrelated real field and failed `read: bad fd for tcp clone`.
    /// Tomcat's `WsFrameClient` reads that as a dropped connection and closes
    /// the session immediately after `onOpen` — the 19-class WebSocket cluster,
    /// from one missing `aio_base`.
    #[test]
    fn every_private_slot_access_goes_through_the_base_aware_accessor() {
        let src = include_str!("async_socket.rs");
        // Split so this test's OWN lines do not contain the pattern it hunts —
        // a self-matching guard reports itself and nothing else.
        let getter = concat!("ctx.get_", "field(this, ");
        let setter = concat!("ctx.set_", "field(this, ");
        let offenders: Vec<(usize, &str)> = src
            .lines()
            .enumerate()
            .filter(|(_, l)| {
                let t = l.trim_start();
                // Skip comments, which quote the bad form deliberately.
                !t.starts_with("//")
                    && (t.contains(getter) || t.contains(setter))
                    && !t.contains("F_")
            })
            .map(|(i, l)| (i + 1, l.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "private slot(s) indexed without `aio_base` — on a CONCRETE receiver \
             these do not address the slot they name, they address whatever real \
             JDK field happens to sit at that index. Use `aio_get`/`aio_set`. \
             Offenders: {offenders:?}"
        );
    }

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
        let roots = HandlerRoots::new(&mut ctx, Some(handler), None);

        push_handler_completion(Completion {
            roots,
            outcome: Ok(CompletionKind::WriteCount { n: 7, buffer_gref }),
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
            "the buffer AND handler global roots must be released after delivery"
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

    // -----------------------------------------------------------------
    // Root audit 2026-08-01 — handler/attachment/channel remapping.
    //
    // Before this wave, `Completion` carried the `CompletionHandler` and its
    // attachment as bare `ObjectRef`s captured on a worker thread *before* a
    // blocking syscall. They were in no root set, so a moving collection was
    // free to relocate them and nothing repointed the copies. Each test below
    // relocates the root between park and drain — the mock's
    // `relocate_global_root` models exactly what the collector's remap pass
    // does — and asserts the delivered reference is the POST-move one. Against
    // the old bare-`ObjectRef` shape the assertion fails with the pre-move
    // address, which is the dangling read this fixes.
    // -----------------------------------------------------------------

    /// `completed()` must be dispatched on the handler's post-relocation
    /// address, and both roots must be released exactly once.
    #[test]
    fn audit_completion_handler_is_remapped_before_delivery() {
        let _g = confine_test_lock().lock();
        let mut ctx = MockNativeContext::new();
        completion_queue().lock().clear();

        let handler_before = ctx.alloc_object(1);
        let attachment_before = ctx.alloc_object(1);
        let roots = HandlerRoots::new(&mut ctx, Some(handler_before), Some(attachment_before));
        push_handler_completion(Completion {
            roots,
            outcome: Ok(CompletionKind::Void),
        });

        // A moving collection runs while the completion sits in the queue.
        let handler_after = ctx.alloc_object(1);
        let attachment_after = ctx.alloc_object(1);
        ctx.relocate_global_root(roots.handler, handler_after);
        ctx.relocate_global_root(roots.attachment, attachment_after);
        assert_ne!(handler_before, handler_after);

        drain_completions(&mut ctx);

        let delivered = ctx
            .recorded_calls()
            .iter()
            .find(|c| c.method_name == "completed")
            .expect("completed must be dispatched")
            .clone();
        assert_eq!(
            delivered.args[0],
            Value::Object(Some(handler_after)),
            "the handler must be resolved through its global root, not from \
             the pre-move ObjectRef the worker captured"
        );
        assert_eq!(
            delivered.args[2],
            Value::Object(Some(attachment_after)),
            "the attachment must be remapped too"
        );
        assert_eq!(
            ctx.global_root_count(),
            0,
            "handler and attachment roots must both be released after delivery"
        );
    }

    /// The `failed()` path takes a different branch (it allocates an
    /// IOException and a message String first, either of which can move the
    /// handler again) and must remap just the same.
    #[test]
    fn audit_failed_delivery_is_remapped_and_releases_roots() {
        let _g = confine_test_lock().lock();
        let mut ctx = MockNativeContext::new();
        completion_queue().lock().clear();

        let handler_before = ctx.alloc_object(1);
        let roots = HandlerRoots::new(&mut ctx, Some(handler_before), None);
        push_handler_completion(Completion {
            roots,
            outcome: Err("connect failed: refused".to_string()),
        });

        let handler_after = ctx.alloc_object(1);
        ctx.relocate_global_root(roots.handler, handler_after);

        drain_completions(&mut ctx);

        let delivered = ctx
            .recorded_calls()
            .iter()
            .find(|c| c.method_name == "failed")
            .expect("failed must be dispatched")
            .clone();
        assert_eq!(
            delivered.args[0],
            Value::Object(Some(handler_after)),
            "failed() must dispatch on the post-move handler"
        );
        // The Throwable itself matters: the arm that builds it is also the arm
        // that can bail out and deliver nothing, so assert what arrived rather
        // than only that something did.
        let Value::Object(Some(thrown)) = delivered.args[1] else {
            panic!("failed() was passed no Throwable: {:?}", delivered.args[1]);
        };
        assert_eq!(
            ctx.class_name_arc_of_id(ctx.class_id_of_object(thrown))
                .as_deref(),
            Some("java/io/IOException"),
            "failed() must be handed a java.io.IOException"
        );
        let Value::Object(Some(message)) = ctx.get_field_by_name(thrown, "detailMessage") else {
            panic!("the IOException carries no detailMessage");
        };
        assert_eq!(
            ctx.read_string(message).as_deref(),
            Some("connect failed: refused"),
            "the worker's failure message must reach the handler"
        );
        assert_eq!(
            ctx.global_root_count(),
            0,
            "the handler root must be released even on the failure path"
        );
    }

    /// A handler-less job still roots its attachment, so the release path must
    /// drop both handles or every such op leaks one root for the life of the VM.
    #[test]
    fn audit_handler_less_job_releases_its_attachment_root() {
        let _g = confine_test_lock().lock();
        let mut ctx = MockNativeContext::new();
        pending_root_releases().lock().clear();

        let attachment = ctx.alloc_object(1);
        let roots = HandlerRoots::new(&mut ctx, None, Some(attachment));
        assert!(!roots.has_handler());
        assert_eq!(ctx.global_root_count(), 1);

        queue_handler_release(roots);
        flush_pending_root_releases(&mut ctx);
        assert_eq!(
            ctx.global_root_count(),
            0,
            "a null CompletionHandler must not strand the attachment's root"
        );
    }

    /// `PendingFieldReset` writes `F_CONNECTED = 0` on a *different* thread
    /// from the one that parked it. It must resolve the channel through its
    /// root: writing through the pre-move `ObjectRef` clears the flag on
    /// whatever object now occupies that address.
    #[test]
    fn audit_pending_field_reset_targets_the_relocated_channel() {
        let _g = confine_test_lock().lock();
        let mut ctx = MockNativeContext::new();
        pending_field_resets().lock().clear();

        let channel_before = ctx.alloc_object(N_FIELDS);
        ctx.set_field(channel_before, F_CONNECTED, Value::Int(1));
        let channel_gref = ctx.add_global_root(channel_before);

        pending_field_resets().lock().push(PendingFieldReset {
            target_gref: channel_gref,
            field: F_CONNECTED,
            value: Value::Int(0),
            release_root: true,
        });

        // The collector moves the channel while the reset is parked. The
        // old address is now a different live object.
        let channel_after = ctx.alloc_object(N_FIELDS);
        ctx.set_field(channel_after, F_CONNECTED, Value::Int(1));
        ctx.relocate_global_root(channel_gref, channel_after);

        flush_pending_field_resets(&mut ctx);

        assert_eq!(
            ctx.get_field(channel_after, F_CONNECTED),
            Value::Int(0),
            "the reset must land on the channel's post-move address"
        );
        assert_eq!(
            ctx.get_field(channel_before, F_CONNECTED),
            Value::Int(1),
            "and must NOT be written through the stale pre-move reference"
        );
        assert_eq!(
            ctx.global_root_count(),
            0,
            "the channel root is released by the last reset for that handle"
        );
    }

    /// The connect-failure path parks TWO resets against one channel root.
    /// Only the last carries `release_root`, so the first must still find the
    /// root live — a premature release would silently drop the second write.
    #[test]
    fn audit_two_field_resets_share_one_channel_root() {
        let _g = confine_test_lock().lock();
        let mut ctx = MockNativeContext::new();
        pending_field_resets().lock().clear();

        let channel = ctx.alloc_object(N_FIELDS);
        ctx.set_field(channel, F_CONNECTED, Value::Int(1));
        ctx.set_field(channel, F_REG_ID, Value::Int(7));
        let channel_gref = ctx.add_global_root(channel);

        {
            let mut resets = pending_field_resets().lock();
            resets.push(PendingFieldReset {
                target_gref: channel_gref,
                field: F_CONNECTED,
                value: Value::Int(0),
                release_root: false,
            });
            resets.push(PendingFieldReset {
                target_gref: channel_gref,
                field: F_REG_ID,
                value: Value::Int(-1),
                release_root: true,
            });
        }
        flush_pending_field_resets(&mut ctx);

        assert_eq!(ctx.get_field(channel, F_CONNECTED), Value::Int(0));
        assert_eq!(
            ctx.get_field(channel, F_REG_ID),
            Value::Int(-1),
            "the second reset must still resolve — the root outlives it"
        );
        assert_eq!(ctx.global_root_count(), 0);
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
            roots: HandlerRoots::default(),
            channel_gref: 0,
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
