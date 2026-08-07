// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP3.4 — Real `sun/nio/ch/SocketChannelImpl` and
//! `sun/nio/ch/ServerSocketChannelImpl` natives with non-blocking
//! (EAGAIN) semantics.
//!
//! These supersede the synthetic stubs that earlier waves provided.
//! The objective is for a pure-Java NIO selector loop driven by the
//! real JDK 25 `sun.nio.ch` classes to bind, accept, read, and write
//! against real OS sockets — including:
//!
//!   * `configureBlocking(false)` switching the underlying TCP socket
//!     to non-blocking mode (so the JDK's selector loop sees `EAGAIN`
//!     / `WSAEWOULDBLOCK` correctly translated to `IOStatus.UNAVAILABLE`).
//!   * `read(ByteBuffer)` returning -1 on EOF, 0 when no data is
//!     available, n on success.
//!   * `write(ByteBuffer)` returning a partial count when the kernel
//!     would block.
//!   * `connect(SocketAddress)` returning false on EINPROGRESS.
//!   * `finishConnect()` polling SO_ERROR / writability.
//!   * Standard TCP options: SO_REUSEADDR, TCP_NODELAY, SO_KEEPALIVE,
//!     SO_RCVBUF, SO_SNDBUF.
//!
//! Storage model:
//!
//!   The Java synthetic objects keep a single `int` slot per channel
//!   pointing into `tcp_registry()`. The real `TcpStream` /
//!   `TcpListener` lives there; on `close()` we drop the entry and
//!   the OS releases the descriptor.
//!
//!   For non-blocking mode we also remember a per-fd flag in
//!   `tcp_blocking_state()`. `std::net::TcpStream::set_nonblocking`
//!   mutates the OS state; the Rust-side flag is the source of truth
//!   for read/write-error mapping (so `WouldBlock` => 0 on Java
//!   bytes-read result rather than an exception).
//!
//! All public surface is registered via `register_socket_channel_real`.

use crate::io_flags;
use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

fn ipc_dbg_enabled() -> bool {
    crate::io_flags().surefire_ipc_dbg
}

fn ipc_dbg(msg: impl AsRef<str>) {
    if ipc_dbg_enabled() {
        eprintln!("[IPC-DBG] {}", msg.as_ref());
    }
}

// ---------------------------------------------------------------------------
// Registry of real OS sockets — keyed by integer id stashed in the synthetic
// SocketChannelImpl / ServerSocketChannelImpl Java object.
// ---------------------------------------------------------------------------

/// A live socket. We separate stream and listener variants because
/// non-blocking semantics differ (accept vs read/write).
pub enum TcpHandle {
    /// A live connection. Held behind an `Arc` so the read/write natives can
    /// clone the handle out of the map and **release the registry lock before
    /// the syscall**. A blocking-mode channel parks inside `read()`/`write()`
    /// for an unbounded time; holding the read lock across that stalls every
    /// later `tcp_register`/`tcp_remove` process-wide, because `parking_lot`'s
    /// `RwLock` parks new readers behind a waiting writer. A client and a
    /// server living in one VM then deadlock outright — Tomcat's
    /// `TestXxxEndpoint.testUnixDomainSocket` does exactly that: the test
    /// thread blocks in `SocketChannel.read` waiting for the response while
    /// the endpoint's acceptor thread is registering the socket it accepted.
    /// (`net.rs`'s `NetSocketHandle::Stream` is `Arc<TcpStream>` for the same
    /// reason.)
    Stream(Arc<TcpStream>),
    /// A client socket after `SocketChannel.bind()` but before `connect()`.
    /// Retaining the actual OS descriptor is essential: the later connect
    /// must keep Hazelcast's requested outbound port instead of silently
    /// opening a different ephemeral socket.
    Bound(TcpStream),
    Listener(TcpListener),
    /// Non-blocking connect in progress. Holds a **real** OS socket whose
    /// `connect()` returned `WSAEWOULDBLOCK` / `EINPROGRESS`. Because it is a
    /// live pollable fd, the selector reports `OP_CONNECT` for it through the
    /// ordinary write-readiness path (see `tcp_clone_for_selector`) — no
    /// manual readiness injection. `finishConnect()` reads `SO_ERROR` via
    /// `nb_connect::poll`. (Earlier waves modelled this with a background
    /// connect-pool thread + completion channel; that entry had no fd, so the
    /// selector never reported `OP_CONNECT` → ES `testAsyncRequests` lost the
    /// request via `CancelledKeyException`.)
    Connecting(TcpStream),
    /// A loopback connect that is known to have failed, retained until the
    /// selector drives `finishConnect()` so Java observes an asynchronous
    /// connect failure rather than a synchronous `connect()` throw.
    ConnectFailed(TcpStream, std::io::Error),
    /// A bound + listening AF_UNIX socket — `ServerSocketChannel.open(UNIX)`
    /// followed by `bind(UnixDomainSocketAddress)`, i.e. Tomcat's
    /// `unixDomainSocketPath` connector. It needs its own variant because
    /// `accept()` has to decode a `sockaddr_un`, which `std`'s `TcpListener`
    /// cannot do. The *accepted* connections are ordinary `Stream` entries —
    /// see `uds.rs` for why an AF_UNIX connection can be carried by a
    /// `TcpStream`.
    UnixListener(crate::uds::UdsListener),
    /// Closed but kept in the map so callers see -1 / -1 idempotently.
    Closed,
}

pub(crate) fn tcp_registry() -> &'static RwLock<HashMap<i32, TcpHandle>> {
    static REG: OnceLock<RwLock<HashMap<i32, TcpHandle>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Try to clone a registered handle out of the tcp_registry. Returns
/// None if the id is unknown / Closed / Connecting. Used by the selector
/// to obtain a `SelectableKind` it can poll without taking ownership of
/// the live JDK-visible handle.
pub(crate) fn tcp_clone_for_selector(id: i32) -> Option<TcpHandleClone> {
    let regs = tcp_registry().read();
    match regs.get(&id) {
        Some(TcpHandle::Listener(l)) => l.try_clone().ok().map(TcpHandleClone::Listener),
        Some(TcpHandle::Stream(s)) => s.try_clone().ok().map(TcpHandleClone::Stream),
        Some(TcpHandle::Bound(s)) => s.try_clone().ok().map(TcpHandleClone::Stream),
        // A connect-in-progress socket is a live pollable fd: clone it as a
        // Stream so the selector polls it for write-readiness and surfaces
        // OP_CONNECT naturally once the OS completes (or refuses) the connect.
        Some(TcpHandle::Connecting(s)) | Some(TcpHandle::ConnectFailed(s, _)) => {
            s.try_clone().ok().map(TcpHandleClone::Stream)
        }
        // AF_UNIX listener: hand the selector the raw OS handle rather than a
        // duplicate. It is only ever *polled*, never owned, and `sc_close`
        // calls `deregister_fd_everywhere(id)` BEFORE dropping the registry
        // entry that closes the socket — so the selector can never be left
        // polling a handle the OS has recycled.
        Some(TcpHandle::UnixListener(l)) => {
            Some(TcpHandleClone::UnixListenerRaw(l.raw() as i64))
        }
        _ => None,
    }
}

pub(crate) enum TcpHandleClone {
    Listener(TcpListener),
    Stream(TcpStream),
    /// Non-owning raw OS handle of an AF_UNIX listener (see above).
    UnixListenerRaw(i64),
}

/// Verdict from probing a registry-resident connecting socket for non-blocking
/// connect completion. Used by the Windows NIO selector to surface `OP_CONNECT`
/// readiness deterministically off the **original** pollable fd, rather than
/// depending on `WSAPoll(POLLOUT)` of the selector's *cloned* handle to fire for
/// the connect-completion edge — which on Windows can be missed entirely when
/// the connect was still in-progress at registration time, parking the selector
/// forever (`nio_selector::kernel_select_windows`).
pub enum SelectorConnectProbe {
    /// Not a connect-in-progress entry (unknown / listener / closed).
    NotConnecting,
    /// Still connecting — neither writable nor errored yet.
    Pending,
    /// Connect completed (or already promoted to a live stream) / failed: the
    /// selector should report `OP_CONNECT` so the reactor calls finishConnect().
    Ready,
}

/// Probe a `tcp_registry` entry by id for non-blocking connect completion.
/// A `Connecting` entry is polled via `nb_connect::poll` on its live socket;
/// a `Stream` entry counts as `Ready` (an immediate/loopback connect already
/// promoted it). Everything else is `NotConnecting`.
pub fn probe_connect_status(net_fd: i32) -> SelectorConnectProbe {
    let failed = {
        let map = tcp_registry().read();
        match map.get(&net_fd) {
            Some(TcpHandle::Connecting(s)) => match crate::nb_connect::poll(s) {
                crate::nb_connect::ConnectPoll::Pending => return SelectorConnectProbe::Pending,
                crate::nb_connect::ConnectPoll::Connected => return SelectorConnectProbe::Ready,
                // `SO_ERROR` is consumptive on Windows. Preserve its first
                // failure below so the reactor's subsequent finishConnect()
                // sees the same ConnectException instead of a false success.
                crate::nb_connect::ConnectPoll::Failed(e) => Some(e),
            },
            Some(TcpHandle::ConnectFailed(_, _)) | Some(TcpHandle::Stream(_)) => {
                return SelectorConnectProbe::Ready;
            }
            _ => return SelectorConnectProbe::NotConnecting,
        }
    };

    if let Some(error) = failed {
        let saved = std::io::Error::new(error.kind(), error.to_string());
        let mut map = tcp_registry().write();
        if let Some(TcpHandle::Connecting(stream)) = map.remove(&net_fd) {
            map.insert(net_fd, TcpHandle::ConnectFailed(stream, saved));
        }
    }
    SelectorConnectProbe::Ready
}

/// Per-fd non-blocking flag. The OS state on the real socket mirrors this.
fn tcp_blocking_state() -> &'static RwLock<HashMap<i32, bool>> {
    static FLAGS: OnceLock<RwLock<HashMap<i32, bool>>> = OnceLock::new();
    FLAGS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Socket options that the real JDK's SocketAdaptor exposes through a
/// SocketChannel. The standard library does not provide portable buffer-size
/// accessors, so retain successful Java-level settings here as the channel's
/// authoritative values. In particular, returning zero for SO_SNDBUF makes
/// Hazelcast allocate a zero-capacity protocol encoder buffer.
fn tcp_option_state() -> &'static RwLock<HashMap<(i32, String), i32>> {
    static OPTIONS: OnceLock<RwLock<HashMap<(i32, String), i32>>> = OnceLock::new();
    OPTIONS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn tcp_next_id() -> i32 {
    // Keep IDs distinct from the `Net::register_handle` counter (which starts
    // at 0x4000_0000) and the UDP / FdId counters (small positives).
    static NEXT: AtomicI32 = AtomicI32::new(0x6000_0000);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

fn tcp_register(h: TcpHandle) -> i32 {
    let id = tcp_next_id();
    tcp_registry().write().insert(id, h);
    id
}

fn tcp_remove(id: i32) {
    tcp_registry().write().remove(&id);
    tcp_blocking_state().write().remove(&id);
    tcp_option_state()
        .write()
        .retain(|(option_id, _), _| *option_id != id);
}

fn tcp_take_bound(id: i32) -> Option<TcpStream> {
    let mut registry = tcp_registry().write();
    let stream = match registry.remove(&id) {
        Some(TcpHandle::Bound(stream)) => Some(stream),
        Some(other) => {
            registry.insert(id, other);
            None
        }
        None => None,
    };
    drop(registry);
    if stream.is_some() {
        tcp_blocking_state().write().remove(&id);
    }
    stream
}

fn tcp_replace_connect_state(id: i32, stream: TcpStream, connected: bool, blocking: bool) {
    let handle = if connected {
        TcpHandle::Stream(Arc::new(stream))
    } else {
        TcpHandle::Connecting(stream)
    };
    tcp_registry().write().insert(id, handle);
    tcp_blocking_state().write().insert(id, blocking);
}

const ACCEPT_CLOSE_POLL: Duration = Duration::from_millis(10);
const LINGERING_CHANNEL_CLOSE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

fn tcp_listener_is_registered(id: i32) -> bool {
    matches!(tcp_registry().read().get(&id), Some(TcpHandle::Listener(_)))
}

/// Accept from the registry's own listener, never from a private duplicate.
///
/// `ServerSocketChannel.close()` must make the port stop listening **by the
/// time it returns** — that is the whole contract a graceful shutdown rests
/// on. `sc_close` implements it by dropping the registry entry, which drops
/// the `TcpListener` and closes the OS socket. That only works if the registry
/// holds the *last* handle.
///
/// This used to `try_clone()` the listener before looping, so an acceptor
/// thread parked here kept a duplicate OS handle alive across the close. Two
/// things went wrong, both of them silent:
///
///   * The port stayed open until the acceptor happened to poll again, so a
///     client connecting in that window completed its TCP handshake instead of
///     being refused.
///   * Worse, the deregistration check sat only in the `WouldBlock` arm — a
///     connection that arrived after the close was returned by `accept()` and
///     SERVED, with no check at all.
///
/// Spring Boot's `JettyServletWebServerFactoryTests
/// .whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade` is
/// exactly that race: it calls `shutDownGracefully` (which closes the
/// connector synchronously) and then connects, expecting
/// `HttpHostConnectException`. It got `404 Not Found` — a real response from
/// the server that was supposed to be closed. It reproduces only under load,
/// because the window is one `ACCEPT_CLOSE_POLL` wide, which is why the test
/// passes standalone and fails inside the full 113-test class.
///
/// Taking the registry read lock per poll instead costs one uncontended lock
/// every 10 ms and makes the close atomic against the accept: `tcp_remove`
/// takes the write lock, so it either runs before an iteration (which then
/// finds no listener and gives up) or after it (which has already returned).
/// The lock is never held across a blocking syscall — the listener is put in
/// non-blocking mode first, so `accept()` here always returns immediately.
fn accept_close_aware(
    id: i32,
    blocking: bool,
) -> std::io::Result<Option<(TcpStream, SocketAddr)>> {
    let mut nonblocking_set = false;
    loop {
        // Scoped so the guard is dropped before the sleep below — otherwise a
        // parked acceptor would hold the registry read lock process-wide.
        let attempt = {
            let map = tcp_registry().read();
            match map.get(&id) {
                Some(TcpHandle::Listener(l)) => {
                    if !nonblocking_set {
                        l.set_nonblocking(true)?;
                        nonblocking_set = true;
                    }
                    Some(l.accept())
                }
                _ => None,
            }
        };
        let Some(result) = attempt else {
            return Err(std::io::Error::new(
                ErrorKind::Interrupted,
                "server channel closed",
            ));
        };
        match result {
            Ok(pair) => return Ok(Some(pair)),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if !blocking {
                    return Ok(None);
                }
                std::thread::sleep(ACCEPT_CLOSE_POLL);
            }
            // Reissue on EINTR rather than reporting it. The Unix-domain
            // sibling below has carried this arm since it was written; this
            // TCP one did not, so a CratonVM cross-thread JIT root-scan
            // `SIGUSR2` landing on a parked accept surfaced as
            // `IOException: Interrupted system call`. See
            // `crate::eintr` for why `SA_RESTART` does not cover it.
            Err(e) if crate::eintr::is_eintr(&e) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// [`accept_close_aware`] with a wall-clock bound. `Ok(None)` means the
/// deadline expired with no connection pending; the caller turns that into
/// `SocketTimeoutException`.
fn accept_until_deadline(
    id: i32,
    deadline: std::time::Instant,
) -> std::io::Result<Option<(TcpStream, SocketAddr)>> {
    loop {
        match accept_close_aware(id, false)? {
            Some(pair) => return Ok(Some(pair)),
            None => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Ok(None);
                }
                std::thread::sleep(remaining.min(ACCEPT_CLOSE_POLL));
            }
        }
    }
}

fn lingering_channel_close(_id: i32, stream: &TcpStream) {
    // EXPERIMENT (2026-07-11, re-test on SB-CRASH-04 fix): shutdown(Write)
    // ONLY, no background drain thread. See task #11 notes for rationale.
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn ioex(msg: impl Into<String>) -> MethodCallFailed {
    RuntimeError::IOException {
        message: msg.into(),
    }
    .into()
}

/// Gated Unix-domain-socket tracing. Shares `CRATONVM_DBG_SC_READ` with the
/// other socket-channel diagnostics in this file: the UDS bind/accept/connect
/// path is only ever interesting alongside the read/write trace.
fn uds_dbg(args: std::fmt::Arguments<'_>) {
    if io_flags().dbg_sc_read {
        eprintln!("[UDS] {args}");
    }
}

/// A null `SocketAddress` handed to a *connect* is a `NullPointerException`,
/// not an `IOException`: the JDK routes every `connect` through
/// `sun.nio.ch.Net.checkAddress`, whose first statement is
/// `Objects.requireNonNull(sa)`. Code that wraps a connect in
/// `catch (IOException)` must not silently absorb its own null-target bug.
/// (`bind(null)`, by contrast, is a legal request for an automatically
/// assigned address — see `sc_bind` / `ssc_bind`.)
fn null_socket_address(op: &str) -> MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some(format!("{op}: null SocketAddress")),
    }
    .into()
}

/// Thrown when a caller asks for a Unix-domain socket on a platform where
/// this build has no AF_UNIX support. Matches the JDK, which raises
/// `UnsupportedOperationException` from `ServerSocketChannel.open(UNIX)`.
fn unsupported_uds() -> MethodCallFailed {
    RuntimeError::UnsupportedOperationException {
        message: "Unix domain sockets are not supported on this platform".into(),
    }
    .into()
}

fn map_err(ctx: &str, e: std::io::Error) -> MethodCallFailed {
    // BUGFIX [nb-socket-channel]: these must throw the CONCRETE `java.net.*`
    // exception types, not a generic `IOException` whose message merely
    // mentions the class name as a text prefix — real code catches them by
    // type (e.g. ES `RestClientMultipleHostsIntegTests.testNodeSelector`
    // does `catch (ConnectException e)` around a connect to a stopped host;
    // a bare IOException escapes that catch and fails the test even though
    // the underlying refusal was detected correctly).
    match e.kind() {
        ErrorKind::ConnectionRefused => RuntimeError::ConnectException {
            // `std::io::Error` uses the localized Winsock text on Windows.
            // Java callers (including Spring Boot's health assertions) rely
            // on the portable `Connection refused` wording, so keep the
            // public exception message stable across host locales.
            message: format!("{ctx}: Connection refused"),
        }
        .into(),
        ErrorKind::TimedOut => RuntimeError::SocketTimeoutException {
            message: format!("{ctx}: {e}"),
        }
        .into(),
        ErrorKind::AddrInUse => RuntimeError::BindException {
            message: format!("Address already in use: {ctx}: {e}"),
        }
        .into(),
        ErrorKind::AddrNotAvailable => RuntimeError::BindException {
            message: format!("Cannot assign requested address: {ctx}: {e}"),
        }
        .into(),
        ErrorKind::PermissionDenied => RuntimeError::BindException {
            message: format!("Permission denied: {ctx}: {e}"),
        }
        .into(),
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset => {
            ioex(format!("SocketException: {ctx}: {e}"))
        }
        ErrorKind::NotConnected => ioex(format!("SocketException: Not connected: {ctx}: {e}")),
        _ => ioex(format!("SocketException: {ctx}: {e}")),
    }
}

/// Throw the real `java.nio.channels.<simple_name>` via its no-arg constructor.
///
/// The CONCRETE class is what matters here, not "some IOException":
/// `RJdkNio.selectorAndAsyncClose` catches `AsynchronousCloseException`, then
/// `ClosedChannelException`, then `IOException`, and records which arm ran —
/// and every real NIO reactor makes the same distinction, because
/// `ClosedChannelException` means "you closed it" while
/// `AsynchronousCloseException` means "someone else closed it under you".
/// A message-prefix convention cannot express that.
///
/// Falls back to a plain `IOException` when the class cannot be built, which is
/// the synthetic-JDK case where `java.nio.channels` may not be present at all.
fn channel_exception(ctx: &mut dyn NativeContext, simple_name: &str) -> MethodCallFailed {
    let class = format!("java/nio/channels/{simple_name}");
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object(&class) {
        // A failing no-arg `<init>` still leaves a usable exception object of
        // the right type; throwing it beats degrading to a generic IOException.
        let _ = ctx.invoke(&class, "<init>", "()V", &[Value::Object(Some(exc))]);
        return MethodCallFailed::ExceptionThrown(exc);
    }
    ioex(format!("{simple_name}: channel closed"))
}

/// `java.nio.channels.ClosedChannelException` — the channel was already closed
/// when this operation started.
fn closed_channel_exception(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    channel_exception(ctx, "ClosedChannelException")
}

/// Map a channel read failure to the exception `java.nio.channels` names for
/// it. `ErrorKind::Interrupted` is [`channel_async_closed_err`]'s signal and
/// nothing else — see its doc comment for why no real EINTR reaches here.
fn closed_or_io_error(
    ctx: &mut dyn NativeContext,
    op: &str,
    e: std::io::Error,
) -> MethodCallFailed {
    if e.kind() == ErrorKind::Interrupted {
        return channel_exception(ctx, "AsynchronousCloseException");
    }
    map_err(op, e)
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn obj_or_none(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn int_arg(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

fn long_arg(args: &[Value], idx: usize) -> i64 {
    match args.get(idx) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    }
}

fn bool_arg(args: &[Value], idx: usize) -> bool {
    matches!(args.get(idx), Some(Value::Int(v)) if *v != 0)
}

/// Allocate a synthetic instance with at least `nfields` slots. Mirrors
/// the `alloc_t16` helper in `nio_native.rs`.
fn alloc_obj(ctx: &mut dyn NativeContext, class_name: &str, nfields: usize) -> ObjectRef {
    match ctx.ensure_class_initialized(class_name) {
        Ok(cid) => {
            // Allocate the FULL real field layout (not just the synthetic
            // `nfields`) so the inherited `AbstractInterruptibleChannel.closeLock`
            // / `AbstractSelectableChannel.keyLock`/`regLock` monitor fields
            // actually exist and can be seeded by `init_channel_locks`. The
            // synthetic per-channel state lives in the `chan_fields` side-table
            // (keyed by identity, see cf_set), so the extra real slots are inert
            // for the natives but let any real channel bytecode that runs (e.g.
            // `close()` reached from the Apache NIO reactor) find non-null locks.
            let real = ctx.class_num_total_fields(cid);
            ctx.alloc_object(cid, real.max(nfields))
        }
        Err(_) => ctx.alloc_object(ClassId::new(0), nfields),
    }
}

/// Seed the `AbstractInterruptibleChannel` / `AbstractSelectableChannel` monitor
/// fields the real JDK `close()`/`register()` bytecode does `synchronized(...)`
/// on. CratonVM creates channels without running those constructors, leaving the
/// `final` locks null → `monitorenter ... null` NPE on any path that reaches the
/// real bytecode. The Apache httpasyncclient I/O reactor closes each session's
/// `SocketChannel` via the final `AbstractInterruptibleChannel.close()` (not the
/// overridable `SocketChannelImpl.close`), so a null `closeLock` killed the
/// reactor worker under load → "I/O reactor has been shut down" (ES
/// testManyAsyncRequests). Idempotent; safe to call on any channel.
///
/// Returns the channel ref, which the caller MUST use from here on: seeding
/// `interruptor` allocates, and an allocation can move `ch`.
#[must_use]
fn init_channel_locks(ctx: &mut dyn NativeContext, ch: ObjectRef) -> ObjectRef {
    // Seed each monitor field with the channel object ITSELF rather than a fresh
    // `new Object()`. The fields only need to be a non-null, stable monitor; the
    // channel is one, and using it avoids the allocation entirely — which matters
    // because `new_object` can trigger a moving GC that relocates `ch`, and under
    // concurrent load (the Apache reactor opening hundreds of channels) that race
    // left `closeLock` null for a few channels → reactor-killing NPE on close (ES
    // testManyAsyncRequests). `synchronized(closeLock)` then `synchronized(keyLock)`
    // both lock the same channel monitor reentrantly (same thread) — correct, and
    // distinct channels still use distinct monitors.
    //
    // These three MUST be seeded before `seed_channel_interruptor` below, which
    // does allocate: if that allocation moves `ch`, a moving GC rewrites the
    // reference stored in these slots along with every other, so a lock seeded
    // first survives the move. A lock seeded after would be written to a stale
    // address instead — which is the exact failure the paragraph above records.
    for f in ["closeLock", "keyLock", "regLock"] {
        if !matches!(ctx.get_field_by_name(ch, f), Value::Object(Some(_))) {
            ctx.set_field_by_name(ch, f, Value::Object(Some(ch)));
        }
    }
    seed_channel_interruptor(ctx, ch)
}

/// Seed `AbstractInterruptibleChannel.interruptor` on a bridge-built channel.
///
/// `interruptor` is a `final` field the real `AbstractInterruptibleChannel()`
/// constructor always assigns, and `begin()` dereferences it unconditionally
/// once `Thread.currentThread().isInterrupted()` is true. CratonVM builds
/// socket channels without running that constructor, so the slot stayed null
/// and ANY channel operation on a thread whose interrupt flag happened to be
/// set died with
/// `NullPointerException: Cannot invoke "sun.nio.ch.Interruptible.interrupt(
/// java.lang.Thread)" because "this.interruptor" is null`
/// instead of performing the specified asynchronous close.
///
/// `sun/nio/ch/FileChannelImpl` was fixed for the H2 `TestStreamStore` NPE on
/// 2026-07-26; the socket channels were left open then because seeding needs an
/// allocation and this path carries an empirically-earned warning against
/// allocating (see `init_channel_locks`). Sequencing the allocation AFTER the
/// three monitor fields, and pinning `ch` across it, removes that objection:
/// nothing that can be left null by a relocation is written afterwards, and the
/// caller is handed the post-GC ref.
///
/// Returns the (possibly relocated) channel ref.
#[must_use]
fn seed_channel_interruptor(ctx: &mut dyn NativeContext, ch: ObjectRef) -> ObjectRef {
    if matches!(ctx.get_field_by_name(ch, "interruptor"), Value::Object(Some(_))) {
        return ch;
    }
    // `new_object_initialized` allocates and runs bytecode, either of which can
    // relocate `ch`; pin it across the call and read the live address back
    // (native stale-local family).
    let pin = ctx.pin_native_root(ch);
    let interruptor = ctx
        .new_object_initialized(
            "java/nio/channels/spi/AbstractInterruptibleChannel$1",
            "(Ljava/nio/channels/spi/AbstractInterruptibleChannel;)V",
            &[Value::Object(Some(ch))],
        )
        .ok()
        .flatten()
        .unwrap_or(Value::Object(None));
    let ch = ctx.read_native_pin(pin, ch);
    ctx.unpin_native_roots(pin);
    // In synthetic-JDK mode the anonymous class does not exist and the
    // construction above fails; writing null is then exactly the previous
    // behaviour, and `set_field_by_name` is a no-op when the slot is absent.
    if matches!(interruptor, Value::Object(Some(_))) {
        ctx.set_field_by_name(ch, "interruptor", interruptor);
    }
    ch
}

/// Layout convention:
///   field 0 = open (1=open, 0=closed)
///   field 1 = blocking (1=blocking, 0=non-blocking)
///   field 2 = registry id (i32 into `tcp_registry`)
///   field 3 = connected (1=connected, 0=not)
///   field 4 = local port (i32, 0=unset)
///   field 5 = remote address text (String or null)
///   field 6 = remote port (i32, 0=unset)
///   field 7 = protocol family (0=INET/INET6, 1=UNIX)
///   field 8 = Unix-domain socket path (String or null; both ends' address)
///   field 9 = input shutdown (1=shut down, 0=open)
///   field 10 = output shutdown (1=shut down, 0=open)
const F_OPEN: usize = 0;
const F_BLOCKING: usize = 1;
const F_REG_ID: usize = 2;
const F_CONNECTED: usize = 3;
const F_LOCAL_PORT: usize = 4;
const F_REMOTE: usize = 5;
const F_REMOTE_PORT: usize = 6;
const F_FAMILY: usize = 7;
const F_UDS_PATH: usize = 8;
const F_INPUT_SHUTDOWN: usize = 9;
const F_OUTPUT_SHUTDOWN: usize = 10;
const N_FIELDS: usize = 11;

/// `F_FAMILY` value for a `StandardProtocolFamily.UNIX` channel.
const FAMILY_UNIX: i32 = 1;

// ---------------------------------------------------------------------------
// Synthetic channel state — identity-hash side-table
// ---------------------------------------------------------------------------
//
// CratonVM now loads the REAL JDK `java.nio.channels.ServerSocketChannel` /
// `sun.nio.ch.SocketChannelImpl` classes. Their low instance slots are
// reference-typed (closeLock, provider, keys, keyLock, ...), so writing our
// synthetic int state (F_REG_ID etc.) into slots 0..6 is silently
// descriptor-coerced to null (`gc::coerce_field_value_by_descriptor`) on both
// read and write — the ids never persisted, surfacing as Tomcat's
// "server channel not bound" (accept) and `getLocalAddress()==null` (bind).
//
// We therefore key the synthetic F_* state to the channel object's GC-stable
// identity hash code instead of its object fields — the same C27 pattern the
// NIO selector key table already uses. The remote host is kept as a Rust
// String so no un-rooted Java ref can go stale under a moving GC.
#[derive(Clone)]
enum Syn {
    I(i32),
    #[allow(dead_code)] // F_REMOTE is write-only today; kept for completeness.
    S(String),
    Null,
}

struct ChanState {
    object: ObjectRef,
    fields: [Syn; N_FIELDS],
}

fn chan_fields() -> &'static RwLock<HashMap<i32, Vec<ChanState>>> {
    static T: OnceLock<RwLock<HashMap<i32, Vec<ChanState>>>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Store a synthetic field. Java String refs are read out to a Rust String
/// (GC-safe); everything else is an int or null.
fn cf_set(ctx: &mut dyn NativeContext, obj: ObjectRef, idx: usize, v: Value) {
    if idx >= N_FIELDS {
        return;
    }
    let slot = match v {
        Value::Int(i) => Syn::I(i),
        Value::Object(Some(s)) => match ctx.read_string(s) {
            Some(rs) => Syn::S(rs),
            None => Syn::Null,
        },
        _ => Syn::Null,
    };
    let key = ctx.identity_hash_code(obj);
    let mut t = chan_fields().write();
    let bucket = t.entry(key).or_default();
    if let Some(state) = bucket.iter_mut().find(|state| state.object == obj) {
        state.fields[idx] = slot;
    } else {
        let mut fields = default_syn();
        fields[idx] = slot;
        bucket.push(ChanState {
            object: obj,
            fields,
        });
    }
}

/// Default synthetic state for a channel object before its `open()`/`accept()`
/// native has populated it: closed, blocking (the JDK default), unbound.
fn default_syn() -> [Syn; N_FIELDS] {
    let mut a: [Syn; N_FIELDS] = std::array::from_fn(|_| Syn::I(0));
    a[F_BLOCKING] = Syn::I(1);
    a
}

/// Read a synthetic int field. Missing / non-int slots read back as `Int(0)`
/// (matching the old absent-field default); `F_REMOTE` is never read.
fn cf_get(ctx: &dyn NativeContext, obj: ObjectRef, idx: usize) -> Value {
    if idx >= N_FIELDS {
        return Value::Int(0);
    }
    let key = ctx.identity_hash_code(obj);
    match chan_fields()
        .read()
        .get(&key)
        .and_then(|bucket| bucket.iter().find(|state| state.object == obj))
        .map(|state| &state.fields[idx])
    {
        Some(Syn::I(i)) => Value::Int(*i),
        _ => Value::Int(0),
    }
}

/// Drop a channel object's synthetic state (called on close) so the table
/// does not grow without bound across short-lived connections.
fn cf_clear(ctx: &dyn NativeContext, obj: ObjectRef) {
    let key = ctx.identity_hash_code(obj);
    let mut table = chan_fields().write();
    let remove_bucket = if let Some(bucket) = table.get_mut(&key) {
        bucket.retain(|state| state.object != obj);
        bucket.is_empty()
    } else {
        false
    };
    if remove_bucket {
        table.remove(&key);
    }
}

/// Cross-module accessor: the `tcp_registry` id backing a synthetic
/// Server/SocketChannel object, or `None` when unbound/unconnected. Used by
/// the NIO selector's register path (`nio_selector::channel_register_native`).
pub fn channel_net_fd(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<i32> {
    match cf_get(ctx, obj, F_REG_ID) {
        Value::Int(v) if v != 0 && v != -1 => Some(v),
        _ => None,
    }
}

/// Read the stored remote `(host, port)` of a connected synthetic channel
/// (the only `Syn::S` slot). `None` when unset.
fn cf_remote(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<(String, i32)> {
    let key = ctx.identity_hash_code(obj);
    let t = chan_fields().read();
    let state = t.get(&key)?.iter().find(|state| state.object == obj)?;
    let host = match &state.fields[F_REMOTE] {
        Syn::S(s) => s.clone(),
        _ => return None,
    };
    let port = match state.fields[F_REMOTE_PORT] {
        Syn::I(p) => p,
        _ => 0,
    };
    Some((host, port))
}

/// Read a `Syn::S` slot (`F_REMOTE` / `F_UDS_PATH`) as a Rust String.
fn cf_get_str(ctx: &dyn NativeContext, obj: ObjectRef, idx: usize) -> Option<String> {
    if idx >= N_FIELDS {
        return None;
    }
    let key = ctx.identity_hash_code(obj);
    let table = chan_fields().read();
    let state = table.get(&key)?.iter().find(|state| state.object == obj)?;
    match &state.fields[idx] {
        Syn::S(s) => Some(s.clone()),
        _ => None,
    }
}

/// True when this channel was opened with `StandardProtocolFamily.UNIX`.
fn is_unix_family(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    matches!(cf_get(ctx, obj, F_FAMILY), Value::Int(FAMILY_UNIX))
}

pub fn gc_scan_channel_roots(roots: &mut Vec<ObjectRef>) {
    let table = chan_fields().read();
    for bucket in table.values() {
        roots.extend(bucket.iter().map(|state| state.object));
    }
}

pub fn channel_fields_update_after_gc<S: std::hash::BuildHasher>(
    pointer_map: &std::collections::HashMap<usize, usize, S>,
) {
    if pointer_map.is_empty() {
        return;
    }
    let mut table = chan_fields().write();
    for bucket in table.values_mut() {
        for state in bucket {
            let old = state.object.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                // SAFETY: the stop-the-world GC supplies a non-null forwarding
                // address for this live ObjectRef before mutators resume.
                state.object = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
}

fn read_reg_id(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    channel_net_fd(ctx, this)
}

fn read_blocking_flag(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    !matches!(cf_get(ctx, this, F_BLOCKING), Value::Int(0))
}

// ---------------------------------------------------------------------------
// SocketAddress decoding
// ---------------------------------------------------------------------------

/// The JDK class name of a Unix-domain socket address.
const UNIX_DOMAIN_SOCKET_ADDRESS: &str = "java/net/UnixDomainSocketAddress";

/// If `sa` is a `java.net.UnixDomainSocketAddress`, return its path as text.
///
/// The class is matched by name rather than by `instanceof` because the
/// address may arrive from any classloader's view of `java.base`; the path is
/// then read through the public `getPath()` / `Path.toString()` accessors so
/// we never depend on the JDK's private field layout.
pub(crate) fn decode_unix_socket_address(
    ctx: &mut dyn NativeContext,
    sa: ObjectRef,
) -> Result<Option<String>, MethodCallFailed> {
    let class_id = ctx.class_id_of_object(sa);
    let Some(name) = ctx.class_name_of_id(class_id) else {
        return Ok(None);
    };
    if name.replace('.', "/") != UNIX_DOMAIN_SOCKET_ADDRESS {
        return Ok(None);
    }
    // Past this point the caller definitely asked for a Unix-domain socket, so
    // a failure to read the path is an error rather than a reason to fall
    // through to the INET decoder (which would report a misleading
    // "unresolved address"). This is the shape a synthetic-JDK run hits, where
    // `java.net.UnixDomainSocketAddress` is a fabricated stub with no real
    // `getPath()`.
    let path = match ctx.invoke_virtual(sa, "getPath", "()Ljava/nio/file/Path;", &[]) {
        Ok(Some(Value::Object(Some(p)))) => p,
        _ => return Err(unsupported_uds()),
    };
    match ctx.invoke_virtual(path, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => match ctx.read_string(s) {
            Some(text) => Ok(Some(text)),
            None => Err(unsupported_uds()),
        },
        _ => Err(unsupported_uds()),
    }
}

/// Build a `java.net.UnixDomainSocketAddress` for `path`, for the
/// `getLocalAddress()` / `getRemoteAddress()` accessors of a UDS channel.
/// Returns `null` if the class is unavailable (e.g. a pre-16 class library).
fn new_unix_socket_address(ctx: &mut dyn NativeContext, path: &str) -> MethodCallResult {
    let text = ctx.create_string(path);
    match ctx.invoke(
        UNIX_DOMAIN_SOCKET_ADDRESS,
        "of",
        "(Ljava/lang/String;)Ljava/net/UnixDomainSocketAddress;",
        &[Value::Object(Some(text))],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
        _ => Ok(Some(Value::Object(None))),
    }
}

/// Decode a `java.net.ProtocolFamily` argument into an `F_FAMILY` value.
/// Anything other than `StandardProtocolFamily.UNIX` (including a null or
/// unreadable argument) maps to the INET default.
fn decode_protocol_family(ctx: &mut dyn NativeContext, args: &[Value], idx: usize) -> i32 {
    let Some(fam) = obj_or_none(args, idx) else {
        return 0;
    };
    let name = match ctx.invoke_virtual(fam, "name", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    };
    if name.as_deref() == Some("UNIX") {
        FAMILY_UNIX
    } else {
        0
    }
}

/// Try to read a socket address out of a Java `InetSocketAddress`-shaped
/// object. The synthetic layout is `(host:String, port:int)`. Real-JDK
/// `InetSocketAddress` is more elaborate (has a holder), but we reach
/// for `getHostString()` / `getPort()` via `get_field_by_name` first.
pub(crate) fn decode_socket_address(
    ctx: &mut dyn NativeContext,
    sa: ObjectRef,
) -> Result<(String, u16), MethodCallFailed> {
    // Preferred path: use the public InetSocketAddress accessors so we
    // observe whatever state the JDK constructor populated, regardless
    // of internal field-layout details.
    let port_via_method = match ctx.invoke_virtual(sa, "getPort", "()I", &[]) {
        Ok(Some(Value::Int(v))) if (0..=u16::MAX as i32).contains(&v) => Some(v as u16),
        _ => None,
    };
    let host_via_method = match ctx.invoke_virtual(sa, "getHostString", "()Ljava/lang/String;", &[])
    {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    };
    if let Some(p) = port_via_method {
        let h = host_via_method.unwrap_or_else(|| "0.0.0.0".to_string());
        let h = if h.is_empty() {
            "0.0.0.0".to_string()
        } else {
            h
        };
        return Ok((h, p));
    }

    // Real-JDK java.net.InetSocketAddress holds its state inside an
    // InetSocketAddressHolder reachable via `holder`. Try that next;
    // fall back to flat field access for synthetic layouts.
    let probe_obj = match ctx.get_field_by_name(sa, "holder") {
        Value::Object(Some(h)) => h,
        _ => sa,
    };
    let port_named = match ctx.get_field_by_name(probe_obj, "port") {
        Value::Int(v) if (0..=u16::MAX as i32).contains(&v) => Some(v as u16),
        _ => None,
    };
    let host_named = match ctx.get_field_by_name(probe_obj, "hostname") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    // Real JDK InetAddress: holder.hostName / holder.address (int IPv4 BE).
    // Synthetic / older shims: hostName / address directly on the InetAddress.
    let addr_named = match ctx.get_field_by_name(probe_obj, "addr") {
        Value::Object(Some(ia)) => {
            let ia_probe = match ctx.get_field_by_name(ia, "holder") {
                Value::Object(Some(h)) => h,
                _ => ia,
            };
            match ctx.get_field_by_name(ia_probe, "hostName") {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => match ctx.get_field_by_name(ia_probe, "address") {
                    Value::Int(v) => Some(format!(
                        "{}.{}.{}.{}",
                        (v >> 24) & 0xff,
                        (v >> 16) & 0xff,
                        (v >> 8) & 0xff,
                        v & 0xff
                    )),
                    _ => None,
                },
            }
        }
        _ => None,
    };
    if let Some(p) = port_named {
        let h = host_named
            .as_ref()
            .or(addr_named.as_ref())
            .map(|s| s.as_str())
            .unwrap_or("0.0.0.0");
        let h = if h.is_empty() { "0.0.0.0" } else { h };
        return Ok((h.to_string(), p));
    }

    // Fall back to synthetic 2-field layout.
    let nf = ctx.object_num_fields(sa);
    if nf >= 2 {
        let host = match ctx.get_field(sa, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let port = match ctx.get_field(sa, 1) {
            Value::Int(v) if (0..=u16::MAX as i32).contains(&v) => v as u16,
            _ => 0,
        };
        if !host.is_empty() {
            return Ok((host, port));
        }
    }

    Err(ioex("SocketAddress: cannot decode host/port"))
}

// ---------------------------------------------------------------------------
// ByteBuffer helpers
// ---------------------------------------------------------------------------

/// Decode the (address, position, limit, capacity, hb-array) view of a
/// `java.nio.ByteBuffer`. Returns either a direct address slice or a
/// heap-array slice. `length` is `limit - position`.
enum BufferAccess {
    /// Direct buffer: `(address, length)` — caller writes to / reads from
    /// the raw memory at `address + position` for `length` bytes.
    Direct { addr: i64, length: i32 },
    /// Heap buffer: `(array_obj, offset, length)`. The caller reads/writes
    /// `length` bytes starting at `array_obj[offset]`.
    Heap {
        arr: ObjectRef,
        offset: i32,
        length: i32,
    },
}

/// Inspect a `ByteBuffer` and return how to access its writable/readable
/// region. Returns None if `bb` is null or unrecognizable.
fn buffer_access(ctx: &mut dyn NativeContext, bb: ObjectRef) -> Option<BufferAccess> {
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

    // Heap: `hb` is the byte[]. Try this first; loading the `address`
    // field on a HeapByteBuffer can transitively trigger class loads
    // (java.lang.foreign.MemorySegment) we don't fully support.
    if let Value::Object(Some(arr)) = ctx.get_field_by_name(bb, "hb") {
        let base_off = match ctx.get_field_by_name(bb, "offset") {
            Value::Int(v) if v >= 0 => v,
            _ => 0,
        };
        return Some(BufferAccess::Heap {
            arr,
            offset: base_off + position,
            length,
        });
    }

    // Direct: `address` is a non-zero long. Only consult it when the
    // heap-array fast path didn't match.
    if let Value::Long(addr) = ctx.get_field_by_name(bb, "address") {
        if addr != 0 {
            return Some(BufferAccess::Direct {
                addr: addr.wrapping_add(position as i64),
                length,
            });
        }
    }

    None
}

/// Bump a buffer's position by `n` bytes after a successful read or write.
fn buffer_advance(ctx: &mut dyn NativeContext, bb: ObjectRef, n: i32) {
    let cur = match ctx.get_field_by_name(bb, "position") {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_field_by_name(bb, "position", Value::Int(cur.saturating_add(n)));
}

/// Materialize an owned byte vector representing the writable/readable
/// region of a buffer. Used for write paths (read from buffer → kernel).
fn buffer_read_bytes(ctx: &mut dyn NativeContext, bb: ObjectRef) -> Option<Vec<u8>> {
    match buffer_access(ctx, bb)? {
        BufferAccess::Direct { addr, length } if addr != 0 && length > 0 => {
            let mut v = vec![0u8; length as usize];
            // `addr` is normally a real `allocateDirect` pointer, but a
            // temp-direct buffer from `Util.getTemporaryDirectBuffer` is an
            // `Unsafe.allocateMemory` arena handle (not dereferenceable).
            // Route through the context so a handle reads from the off-heap
            // store instead of a raw memcpy that would SIGSEGV; for a real
            // pointer this is the same `copy_nonoverlapping`.
            if !ctx.copy_from_native_memory(addr, &mut v) {
                return Some(Vec::new());
            }
            Some(v)
        }
        BufferAccess::Heap {
            arr,
            offset,
            length,
        } if length > 0 => {
            // Bulk read via NativeContext intrinsic. The old element-by-element
            // loop clamped `length` to whatever fit before `arr_len`; preserve
            // that by clamping the effective length here.
            let arr_len = ctx.array_length(arr);
            let off = offset as usize;
            let avail = arr_len.saturating_sub(off);
            let eff_len = (length as usize).min(avail);
            let mut v = vec![0u8; eff_len];
            ctx.read_byte_array_into(arr, off, &mut v);
            Some(v)
        }
        _ => Some(Vec::new()),
    }
}

/// Write `data` into the buffer's writable region (read paths: kernel → buffer).
/// Returns the actual number of bytes written (capped by buffer length).
fn buffer_write_bytes(ctx: &mut dyn NativeContext, bb: ObjectRef, data: &[u8]) -> i32 {
    let access = match buffer_access(ctx, bb) {
        Some(a) => a,
        None => return 0,
    };
    match access {
        BufferAccess::Direct { addr, length } if addr != 0 && length > 0 => {
            let n = (data.len() as i32).min(length).max(0);
            // Route through the context: `addr` may be a temp-direct arena
            // handle (see buffer_read_bytes). A handle writes to the off-heap
            // store; a real pointer falls through to a raw copy.
            if !ctx.copy_to_native_memory(addr, &data[..n as usize]) {
                return 0;
            }
            n
        }
        BufferAccess::Heap {
            arr,
            offset,
            length,
        } if length > 0 => {
            // Bulk write via NativeContext intrinsic. The old loop wrote at
            // most `min(data.len(), length)` bytes, stopping early if it ran
            // past `arr_len`; clamp the effective length the same way.
            let arr_len = ctx.array_length(arr);
            let off = offset as usize;
            let avail = arr_len.saturating_sub(off);
            let n = (data.len() as i32).min(length).max(0).min(avail as i32);
            ctx.write_byte_array_from(arr, off, &data[..n as usize]);
            n
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Public — open / close / configureBlocking / setOption (shared)
// ---------------------------------------------------------------------------

/// `SocketChannel.open()` — allocate a fresh client channel. We don't bind
/// or connect yet; that happens on `connect`.
fn sc_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    sc_open_family_value(ctx, 0)
}

/// Shared body of `SocketChannel.open()` / `open(ProtocolFamily)`.
fn sc_open_family_value(ctx: &mut dyn NativeContext, family: i32) -> MethodCallResult {
    let ch = alloc_obj(ctx, "java/nio/channels/SocketChannel", N_FIELDS);
    let ch = init_channel_locks(ctx, ch);
    cf_set(ctx, ch, F_OPEN, Value::Int(1));
    cf_set(ctx, ch, F_BLOCKING, Value::Int(1));
    cf_set(ctx, ch, F_REG_ID, Value::Int(-1));
    cf_set(ctx, ch, F_CONNECTED, Value::Int(0));
    cf_set(ctx, ch, F_LOCAL_PORT, Value::Int(0));
    cf_set(ctx, ch, F_REMOTE, Value::Object(None));
    cf_set(ctx, ch, F_REMOTE_PORT, Value::Int(0));
    cf_set(ctx, ch, F_FAMILY, Value::Int(family));
    Ok(Some(Value::Object(Some(ch))))
}

/// `SocketChannel.open(ProtocolFamily)` (JDK 16+). Only the family is
/// remembered here — an AF_UNIX socket is created by `connect()`, exactly as
/// the INET path defers its socket to `connect()`.
fn sc_open_family(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let family = decode_protocol_family(ctx, args, 0);
    if family == FAMILY_UNIX && !crate::uds::is_supported() {
        return Err(unsupported_uds());
    }
    sc_open_family_value(ctx, family)
}

/// `SocketChannel.open(SocketAddress)` — open and immediately connect.
fn sc_open_connected(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let ch_val = sc_open(ctx, &[])?;
    let ch = match ch_val {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(ch_val),
    };
    if let Some(sa) = obj_or_none(args, 0) {
        // Synchronous-connect overload of SocketChannel.open(SocketAddress)
        // is documented to throw IOException on failure. Surface the error
        // so callers can react instead of getting an unconnected channel.
        sc_connect_inner(ctx, ch, sa, /* allow_block = */ true)?;
    }
    Ok(Some(Value::Object(Some(ch))))
}

fn sc_is_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) => Ok(Some(cf_get(ctx, o, F_OPEN))),
        _ => Ok(Some(Value::Int(0))),
    }
}

fn sc_is_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) => Ok(Some(Value::Int(if read_blocking_flag(ctx, o) {
            1
        } else {
            0
        }))),
        _ => Ok(Some(Value::Int(1))),
    }
}

/// `isBound()Z` for a (server) socket channel. Not a method on the abstract
/// `java.nio.channels.ServerSocketChannel`, but `sun.nio.ch.ServerSocketAdaptor`
/// (returned by our `ssc_socket`) and Netty's `NioServerSocketChannel.isActive()`
/// call `isBound()` on the channel/adaptor. We track "bound" as "a non-zero local
/// port has been assigned" — `ssc_bind` sets `F_LOCAL_PORT` to the actual bound
/// port (never 0 on success), and `ssc_open` initializes it to 0.
fn ssc_is_bound(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) => {
            // A Unix-domain listener has no port at all, so "bound" is
            // "a listener id has been registered" for that family instead.
            let bound = if is_unix_family(ctx, o) {
                read_reg_id(ctx, o).is_some()
            } else {
                cf_get(ctx, o, F_LOCAL_PORT).as_int().unwrap_or(0) > 0
            };
            Ok(Some(Value::Int(if bound { 1 } else { 0 })))
        }
        _ => Ok(Some(Value::Int(0))),
    }
}

fn sc_is_connected(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) => Ok(Some(cf_get(ctx, o, F_CONNECTED))),
        _ => Ok(Some(Value::Int(0))),
    }
}

/// `SocketChannel.isConnectionPending()` -- real HotSpot's `SocketChannelImpl`
/// gives this a concrete body (`state == ST_PENDING`), so it is NOT declared
/// `native` and CratonVM never registered it: dispatch on our synthetic
/// `SocketChannel` object fell through to the abstract declaration on
/// `java.nio.channels.SocketChannel` (no Code attribute) ->
/// `AbstractMethodError`. That `Error` (not `Exception`) is invisible to
/// HttpClient5's `InternalChannel.handleIOEvent`, whose `catch (Exception ex)`
/// does not catch it, and to `IOReactorWorker.run()`'s own `catch (Exception
/// e)` -- so it silently kills the reactor worker thread with no log, no
/// stored throwable, and no callback ever firing. This is the same defect
/// family as the `supportedOptions()` `AbstractMethodError` fixed earlier
/// (native-io/src/socket_channel.rs), just one call further down the
/// connect-completion handoff: `InternalConnectChannel.onIOEvent` calls
/// `isConnectionPending()` as its very first step, before `finishConnect()`.
///
/// True iff the channel is registered in the `tcp_registry` as a
/// `Connecting` entry (real non-blocking connect started, not yet completed
/// via `finishConnect()`/promoted to a `Stream`) and not already marked
/// connected.
fn sc_is_connection_pending(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    if matches!(cf_get(ctx, this, F_CONNECTED), Value::Int(1)) {
        return Ok(Some(Value::Int(0)));
    }
    let id = match read_reg_id(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))),
    };
    // `ConnectFailed` is also "pending" from Java's point of view: connect()
    // returned `false` for it (see `sc_connect_inner`/`sc_connect_bound`) and
    // the failure is only observable via `finishConnect()`, exactly like a
    // still-in-flight `Connecting` entry — a reactor that checks
    // `isConnectionPending()` before calling `finishConnect()` must see
    // `true` here or it will never make the call that reports the error.
    let pending = matches!(
        tcp_registry().read().get(&id),
        Some(TcpHandle::Connecting(_)) | Some(TcpHandle::ConnectFailed(_, _))
    );
    Ok(Some(Value::Int(if pending { 1 } else { 0 })))
}

/// `SocketChannelImpl.isInputOpen()` / `isOutputOpen()` (package-private) —
/// consulted by sun.nio.ch.SocketAdaptor's input/output streams (the streams
/// returned by socket().getInputStream()/getOutputStream()).
fn sc_io_open(ctx: &mut dyn NativeContext, args: &[Value], shutdown_field: usize) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) => {
            let open = matches!(cf_get(ctx, o, F_OPEN), Value::Int(1));
            let shut_down = matches!(cf_get(ctx, o, shutdown_field), Value::Int(1));
            Ok(Some(Value::Int(if open && !shut_down { 1 } else { 0 })))
        }
        _ => Ok(Some(Value::Int(0))),
    }
}

/// `SocketChannel.shutdownInput()` / `shutdownOutput()`.
///
/// Apache HttpComponents half-closes the request side after it has written an
/// HTTP request.  The abstract declaration has no Code attribute, so leaving
/// it unregistered terminates the reactor thread with AbstractMethodError and
/// strands its CompletableFuture.  Apply the actual TCP half-close and retain
/// the state for the package-private `is{Input,Output}Open` accessors.
fn sc_shutdown(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    how: std::net::Shutdown,
    shutdown_field: usize,
    operation: &str,
) -> MethodCallResult {
    let this = obj_or_none(args, 0).ok_or_else(|| ioex(format!("{operation}: null channel")))?;
    let id = read_reg_id(ctx, this)
        .ok_or_else(|| ioex(format!("{operation}: channel not connected")))?;
    match resolve_stream(id) {
        StreamTarget::Ready(stream) => stream.shutdown(how).map_err(|e| map_err(operation, e))?,
        StreamTarget::Connecting => return Err(ioex(format!("{operation}: channel not connected"))),
        StreamTarget::Failed(error) => return Err(map_err(operation, error)),
        StreamTarget::Unavailable => return Err(ioex(format!("{operation}: channel is closed"))),
    }
    cf_set(ctx, this, shutdown_field, Value::Int(1));
    Ok(Some(Value::Object(Some(this))))
}

fn sc_configure_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let blocking = bool_arg(args, 1);
    cf_set(
        ctx,
        this,
        F_BLOCKING,
        Value::Int(if blocking { 1 } else { 0 }),
    );
    if let Some(id) = read_reg_id(ctx, this) {
        // Apply to the live socket — both stream and listener support it.
        let map = tcp_registry().read();
        let res = match map.get(&id) {
            Some(TcpHandle::Stream(s)) => s.set_nonblocking(!blocking),
            Some(TcpHandle::Listener(l)) => l.set_nonblocking(!blocking),
            Some(TcpHandle::UnixListener(l)) => l.set_nonblocking(!blocking),
            _ => Ok(()),
        };
        drop(map);
        if let Err(e) = res {
            return Err(map_err("configureBlocking", e));
        }
        tcp_blocking_state().write().insert(id, blocking);
    }
    Ok(Some(Value::Object(Some(this))))
}

fn sc_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(this) = obj_or_none(args, 0) {
        if let Some(id) = read_reg_id(ctx, this) {
            // Diagnostic (CRATONVM_DBG_SC_CLOSE=1, added 2026-07-16 during the
            // StompWebSocketIntegrationTests investigation): trace every
            // SocketChannel.close() with local/peer address + wall-clock time.
            // Confirmed the server side closes a just-upgraded WebSocket
            // connection (via this exact native) within ~40ms-2s of the
            // handshake completing, before the client's first post-handshake
            // frame write — root cause of that class's TIMEOUT still open, see
            // CRATONVM-SPRING-GENUINE-BUGLIST. Kept as a
            // permanent opt-in hook (zero cost when unset) for whoever
            // continues that investigation, matching CRATONVM_DBG_NET /
            // CRATONVM_DBG_STALE_RECV etc.
            if io_flags().dbg_sc_close {
                let (local, peer) = match tcp_registry().read().get(&id) {
                    Some(TcpHandle::Stream(s)) => (
                        s.local_addr().map(|a| a.to_string()).unwrap_or_default(),
                        s.peer_addr().map(|a| a.to_string()).unwrap_or_default(),
                    ),
                    _ => (String::new(), String::new()),
                };
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                eprintln!("[SC_CLOSE] t={ms} id={id:#x} local={local} peer={peer}");
                // 2026-07-16 follow-up: pin the exact Java call site issuing
                // this close(). `NativeContext::capture_stack_trace` needs no
                // `Thread` object handle -- it walks the CURRENT thread's live
                // Java call stack, which is exactly the thread executing this
                // native (the one that called SocketChannel.close()). Gated
                // behind the same env var; printed innermost-frame-first (the
                // `close()` caller itself first, working outward) to match
                // conventional stack-trace reading order -- `capture_stack_trace`
                // itself returns outer->inner, so reverse it here.
                let raw_trace = ctx.capture_stack_trace(0);
                eprintln!(
                    "[SC_CLOSE_STACK] t={ms} id={id:#x} ({} frames)",
                    raw_trace.len()
                );
                for entry in raw_trace.iter().rev() {
                    let file = entry.source_file.as_deref().unwrap_or("?");
                    eprintln!(
                        "  at {}.{}({}:{})",
                        entry.class_name, entry.method_name, file, entry.line_number
                    );
                }
            }
            // Force the write-side FIN now. A selector this channel was
            // registered with holds a `try_clone()`d duplicate of the socket
            // (see `nio_selector::selector_register`); on Windows, closing only
            // the original handle (the `tcp_remove` below) does NOT shut the
            // connection while that duplicate is alive, so the peer's blocking
            // read never sees EOF and hangs forever. Do not use
            // `Shutdown::Both`: if the peer is still sending request-body bytes,
            // aborting the read side is RST-prone on Windows and surfaces to the
            // client as WSAECONNABORTED instead of the graceful close Tomcat's
            // swallow-input path expects.
            {
                let map = tcp_registry().read();
                if let Some(TcpHandle::Stream(s)) = map.get(&id) {
                    lingering_channel_close(id, s);
                }
            }
            // Drop the selector's cloned handle too, mirroring the JDK where
            // closing a channel cancels its keys — otherwise the poller keeps a
            // live duplicate of a logically-closed socket. Done after dropping
            // the tcp_registry lock above to keep the `selectors → tcp_registry`
            // lock order of the select path (no inversion).
            crate::nio_selector::deregister_fd_everywhere(id);
            tcp_remove(id);
        }
        // Drop the synthetic state entirely: a later isOpen()/isConnected()
        // then reads the default Int(0) (== closed/not-connected), and the
        // side-table does not grow across many short-lived connections.
        cf_clear(ctx, this);
    }
    Ok(None)
}

/// `SocketChannel.socket()` — return a `java.net.Socket` adapter. This is an
/// abstract method on `java.nio.channels.SocketChannel` (no Code attribute),
/// so without this native Tomcat's `NioEndpoint.setSocketOptions` hits an
/// `AbstractMethodError`. The adapter is only used by callers to set socket
/// options (`socketProperties.setProperties(socket)`); those setters dispatch
/// to the synthetic `java/net/Socket` natives and no-op gracefully (their
/// stream id slot reads -1). Real I/O keeps flowing through the channel.
fn sc_socket(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("socket: null channel")),
    };
    // A Unix-domain channel has no `java.net.Socket` view: the real
    // `SocketChannelImpl.socket()` throws `UnsupportedOperationException` for
    // any non-INET family, and a `SocketAdaptor` built over one would hand out
    // `InetSocketAddress`-shaped answers that do not exist. Tomcat guards its
    // own `socket()` calls on `getUnixDomainSocketPath() == null`; match the
    // JDK so anything that does not guard fails the same way it would there.
    if is_unix_family(ctx, this) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: "Not supported".into(),
        }
        .into());
    }
    // Mirror the real `SocketChannelImpl.socket()` → `SocketAdaptor.create(this)`:
    // the adaptor is a proper `java.net.Socket` subclass whose option getters
    // (`getKeepAlive`/`getTcpNoDelay`/…), `connect`, `getInputStream`/
    // `getOutputStream` are OVERRIDDEN to delegate to the channel — so they never
    // touch `Socket.getImpl()` / the `socketLock` monitor. A bare
    // `new java/net/Socket` (no `<init>`) leaves `socketLock` (a `final Object`
    // instance-initializer field) null, so the FIRST real `Socket` method that
    // does `synchronized (socketLock)` — e.g. `getKeepAlive()`→`getImpl()` —
    // throws "monitorenter ... null". The Apache httpasyncclient I/O reactor
    // (`BaseIOReactor`) inspects `channel.socket()` options on every accepted
    // session, so that NPE kills the reactor worker → every request's future
    // hangs (ES-HANG-02). The adaptor is correct regardless of the
    // `CRATONVM_REAL_NET_SOCKETS` gate, so build it unconditionally.
    match ctx.invoke(
        "sun/nio/ch/SocketAdaptor",
        "create",
        "(Lsun/nio/ch/SocketChannelImpl;)Ljava/net/Socket;",
        &[Value::Object(Some(this))],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => return Ok(Some(v)),
        // Fall through to the bare-Socket fallback on any failure so callers
        // still get an object.
        _ => {}
    }
    let sock = ctx
        .new_object("java/net/Socket")
        .ok()
        .and_then(|v| match v {
            Some(Value::Object(Some(o))) => Some(o),
            _ => None,
        })
        .ok_or_else(|| ioex("socket: could not allocate Socket"))?;
    // Safety net: the bare Socket skipped <init>, so seed `socketLock` with a
    // live monitor object so any `synchronized (socketLock)` method doesn't NPE.
    if !matches!(
        ctx.get_field_by_name(sock, "socketLock"),
        Value::Object(Some(_))
    ) {
        if let Ok(Some(Value::Object(Some(lock)))) = ctx.new_object("java/lang/Object") {
            ctx.set_field_by_name(sock, "socketLock", Value::Object(Some(lock)));
        }
    }
    Ok(Some(Value::Object(Some(sock))))
}

/// `java.net.Socket` option setters, for the `Socket` handed out by
/// `SocketChannel.socket()`.
///
/// These used to be a blanket constant `Ok(None)`. The intent was narrow — the
/// bare-adapter `Socket` (the `SocketAdaptor.create` fallback in [`sc_socket`])
/// has no real `SocketImpl`, so the real setter bytecode would go through
/// `getImpl()` — but the registry is keyed on the CLASS NAME, so the no-op
/// caught EVERY `java.net.Socket` in the VM, and `register_io_natives` runs
/// after `register_essential_natives_with_shims` (see `vm/src/vm/vm_init.rs`),
/// so last-writer-wins handed it the slot for all of them.
/// `socket.setTcpNoDelay(true)` was therefore discarded process-wide while the
/// matching getters in `net_phase_e` reported the socket's true (unset) state.
///
/// Now: when the receiver is one of THIS module's channel-backed sockets the
/// option is applied for real and recorded in `tcp_option_state`, so
/// `setTcpNoDelay(true)` / `getTcpNoDelay()` round-trips through
/// [`sc_get_option`]. Anything else is left alone — see
/// [`socket_opt_apply`]'s note on the plain-`Socket` case.
fn socket_opt_apply(ctx: &mut dyn NativeContext, args: &[Value], name: &str) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    let id = match read_reg_id(ctx, this) {
        Some(id) => id,
        // Plain `java.net.Socket`: its live `TcpStream` lives in
        // `cratonvm-native-builtins` (`servlet::s2_registry`, written by
        // `net_phase_e`'s `Socket.connect`/`ServerSocket.accept`), which this
        // crate cannot reach — `native-builtins` depends on `native-io`, not
        // the other way round. Accepting and ignoring is still strictly better
        // than dropping the registration: without one, real `Socket` bytecode
        // runs `getImpl()` on a null `impl`, takes `createImpl(true)` and
        // manufactures a throwaway OS socket to apply the option to, leaking an
        // fd per call (the same defect `net_phase_e` just fixed on the getter
        // side). The registration loop below no longer shadows a real setter if
        // an earlier registrar provided one, so this arm disappears for any key
        // that gets a proper implementation.
        None => return Ok(None),
    };
    let val = socket_option_value(ctx, args.get(1).copied().unwrap_or(Value::Int(0)));
    tcp_option_state()
        .write()
        .insert((id, name.to_string()), val);
    let map = tcp_registry().read();
    // `Stream` holds an `Arc<TcpStream>` and `Bound` a plain `TcpStream`, so
    // the two cannot share an or-pattern binding — reborrow each to `&TcpStream`.
    let stream: Option<&TcpStream> = match map.get(&id) {
        Some(TcpHandle::Stream(s)) => Some(s.as_ref()),
        Some(TcpHandle::Bound(s)) => Some(s),
        _ => None,
    };
    if let Some(s) = stream {
        if let Err(e) = apply_option(s, name, val) {
            return Err(map_err(&format!("setOption({name})"), e));
        }
    }
    Ok(None)
}

fn socket_opt_set_rcvbuf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    socket_opt_apply(ctx, args, "SO_RCVBUF")
}

fn socket_opt_set_sndbuf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    socket_opt_apply(ctx, args, "SO_SNDBUF")
}

fn socket_opt_set_keepalive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    socket_opt_apply(ctx, args, "SO_KEEPALIVE")
}

fn socket_opt_set_reuseaddr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    socket_opt_apply(ctx, args, "SO_REUSEADDR")
}

fn socket_opt_set_nodelay(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    socket_opt_apply(ctx, args, "TCP_NODELAY")
}

fn socket_opt_set_oobinline(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    socket_opt_apply(ctx, args, "SO_OOBINLINE")
}

fn socket_opt_set_linger(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `setSoLinger(boolean on, int linger)` — arg 1 is the flag, which is what
    // `apply_option`/`read_option` key on.
    socket_opt_apply(ctx, args, "SO_LINGER")
}

/// `setPerformancePreferences(int,int,int)` is advisory in the JDK too — the
/// spec says an implementation is free to ignore the hint entirely, and HotSpot
/// ignores it for a connected socket. The no-op IS the behaviour, not a stub.
fn socket_opt_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// `SocketChannel.getRemoteAddress()` — build a real `InetSocketAddress` from
/// the stored peer host/port (Tomcat's `NioSocketWrapper.populateRemoteAddr`
/// calls this then `getAddress().getHostAddress()`, so a synthetic 2-field
/// object would not do). Returns null when not connected.
fn sc_remote_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    if is_unix_family(ctx, this) {
        // Both ends of a UDS connection report the server's path: the client
        // socket is unnamed, which is exactly what the JDK surfaces too.
        let path = cf_get_str(ctx, this, F_UDS_PATH).unwrap_or_default();
        return new_unix_socket_address(ctx, &path);
    }
    if let Some((host, port)) = cf_remote(ctx, this) {
        if port > 0 {
            return new_resolved_inet_socket_address(ctx, &host, port);
        }
    }
    Ok(Some(Value::Object(None)))
}

/// `SocketChannel.getLocalAddress()` — build a real `InetSocketAddress` from
/// the live stream's local end. Returns null when not connected.
fn sc_local_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    if is_unix_family(ctx, this) {
        let path = cf_get_str(ctx, this, F_UDS_PATH).unwrap_or_default();
        return new_unix_socket_address(ctx, &path);
    }
    let id = match read_reg_id(ctx, this) {
        Some(i) => i,
        None => return Ok(Some(Value::Object(None))),
    };
    let local = {
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Stream(s)) => s.local_addr().ok(),
            Some(TcpHandle::Bound(s)) => s.local_addr().ok(),
            _ => None,
        }
    };
    let Some(addr) = local else {
        return Ok(Some(Value::Object(None)));
    };
    new_resolved_inet_socket_address(ctx, &addr.ip().to_string(), addr.port() as i32)
}

/// `SocketChannel.bind(SocketAddress)`: create and retain a real bound client
/// socket. Hazelcast binds its `SocketChannel.socket()` before handing the
/// channel to its NIO connector; keeping this descriptor is what makes the
/// later connect honour a configured outbound-port range.
fn sc_bind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_or_none(args, 0).ok_or_else(|| ioex("bind: null channel"))?;
    if read_reg_id(ctx, this).is_some() {
        return Err(ioex("bind: channel is already bound or connected"));
    }
    // `SocketChannel.bind(null)` is SPECIFIED as "bind to an address that is
    // assigned automatically" (the wording `NetworkChannel.bind` uses for
    // every channel type). Rejecting the null turned the documented "let the
    // OS pick my local endpoint" call into an IOException.
    let bind_text = match obj_or_none(args, 1) {
        None => "0.0.0.0:0".to_string(),
        Some(sa) => {
            // Binding the *client* end of a Unix-domain connection to an
            // explicit path (giving the socket a name of its own) is a JDK
            // capability nothing in the supported workloads uses, and the INET
            // decoder below would mangle the address into a bogus host:port.
            // Reject it plainly instead. An unnamed client socket — the normal
            // case, and what `connect()` produces — needs no bind at all.
            if decode_unix_socket_address(ctx, sa)?.is_some() {
                return Err(RuntimeError::UnsupportedOperationException {
                    message:
                        "Binding a Unix domain SocketChannel to an explicit path is not supported"
                            .into(),
                }
                .into());
            }
            let (host, port) = decode_socket_address(ctx, sa)?;
            if host.is_empty() {
                format!("0.0.0.0:{port}")
            } else {
                format!("{host}:{port}")
            }
        }
    };
    let bind_addr = bind_text
        .to_socket_addrs()
        .map_err(|e| map_err(&bind_text, e))?
        .next()
        .ok_or_else(|| ioex(format!("bind: no addresses resolved for {bind_text}")))?;
    let stream = crate::nb_connect::bind(&bind_addr).map_err(|e| map_err(&bind_text, e))?;
    let local_port = stream
        .local_addr()
        .map(|a| a.port() as i32)
        .unwrap_or(bind_addr.port() as i32);
    let blocking = read_blocking_flag(ctx, this);
    let id = tcp_register(TcpHandle::Bound(stream));
    tcp_blocking_state().write().insert(id, blocking);
    cf_set(ctx, this, F_REG_ID, Value::Int(id));
    cf_set(ctx, this, F_LOCAL_PORT, Value::Int(local_port));
    Ok(Some(Value::Object(Some(this))))
}

fn sc_connect_bound(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    id: i32,
    stream: TcpStream,
    host: &str,
    port: u16,
    allow_block: bool,
) -> Result<bool, MethodCallFailed> {
    let target = format!("{host}:{port}");
    let local = stream
        .local_addr()
        .map_err(|e| map_err("bound local address", e))?;
    let remote = resolve_and_vet(&target)?
        .into_iter()
        .find(|addr| addr.is_ipv4() == local.is_ipv4())
        .ok_or_else(|| {
            ioex(format!(
                "connect: {target} has no address compatible with bound {local}"
            ))
        })?;
    let blocking = read_blocking_flag(ctx, this);
    let started =
        crate::nb_connect::start_bound(stream, &remote).map_err(|e| map_err(&target, e))?;
    let (mut stream, connected) = match started {
        crate::nb_connect::StartConnect::Connected(stream) => (stream, true),
        crate::nb_connect::StartConnect::InProgress(stream) if !allow_block => (stream, false),
        crate::nb_connect::StartConnect::DeferredFailure(stream, error) if !allow_block => {
            // Keep the terminal error on the retained OS descriptor. This
            // mirrors the unbound non-blocking path: the Java reactor must
            // reach finishConnect()/its first write and observe the saved
            // failure, rather than receiving a synchronous connect exception.
            //
            // Do NOT mark this connected — see the matching comment in
            // `sc_connect_inner`'s deferred-failure branch. Returning `true`
            // here made Netty's connect-completion fast path treat a refused
            // loopback connect as an immediate success, surfacing the real
            // failure only as a raw "Connection refused" on the first write
            // instead of a typed exception from `finishConnect()`.
            tcp_registry()
                .write()
                .insert(id, TcpHandle::ConnectFailed(stream, error));
            tcp_blocking_state().write().insert(id, false);
            cf_set(ctx, this, F_LOCAL_PORT, Value::Int(local.port() as i32));
            let host_str = ctx.create_string(host);
            cf_set(ctx, this, F_REMOTE, Value::Object(Some(host_str)));
            cf_set(ctx, this, F_REMOTE_PORT, Value::Int(port as i32));
            return Ok(false);
        }
        crate::nb_connect::StartConnect::DeferredFailure(_stream, error) => {
            return Err(map_err(&target, error));
        }
        crate::nb_connect::StartConnect::InProgress(stream) => {
            let deadline = std::time::Instant::now() + crate::outbound_policy::connect_timeout();
            ctx.begin_blocking_region();
            let verdict = loop {
                match crate::nb_connect::poll(&stream) {
                    crate::nb_connect::ConnectPoll::Connected => break Ok(()),
                    crate::nb_connect::ConnectPoll::Failed(e) => break Err(e),
                    crate::nb_connect::ConnectPoll::Pending
                        if std::time::Instant::now() >= deadline =>
                    {
                        break Err(std::io::Error::new(
                            ErrorKind::TimedOut,
                            "bound connect timed out",
                        ));
                    }
                    crate::nb_connect::ConnectPoll::Pending => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                }
            };
            ctx.end_blocking_region();
            verdict.map_err(|e| map_err(&target, e))?;
            (stream, true)
        } // NOTE: a second `DeferredFailure` arm here would be unreachable —
          // the unconditional `DeferredFailure(_stream, error) => return
          // Err(...)` arm above already matches every case this one used to
          // guard on (`allow_block == true`, since the `if !allow_block` arm
          // earlier in this match consumes the `false` case). The compiler
          // flagged the old duplicate arm as a hard unreachable-pattern
          // warning; removed rather than left as dead code.
    };
    if connected && blocking {
        stream
            .set_nonblocking(false)
            .map_err(|e| map_err("set_nonblocking", e))?;
    }
    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    tcp_replace_connect_state(id, stream, connected, blocking);
    cf_set(
        ctx,
        this,
        F_CONNECTED,
        Value::Int(if connected { 1 } else { 0 }),
    );
    cf_set(ctx, this, F_LOCAL_PORT, Value::Int(local_port));
    let host_str = ctx.create_string(host);
    cf_set(ctx, this, F_REMOTE, Value::Object(Some(host_str)));
    cf_set(ctx, this, F_REMOTE_PORT, Value::Int(port as i32));
    Ok(connected)
}

// ---------------------------------------------------------------------------
// SocketChannel.connect / finishConnect
// ---------------------------------------------------------------------------

/// H3b: resolve a `host:port` target to one or more `SocketAddr`s and
/// vet every resolved address against the outbound-host policy, mirroring
/// `outbound_policy::policy_connect`'s resolution loop. The blocking path
/// gets this for free via `policy_connect`; the non-blocking path calls
/// this so its downstream dial (`nb_connect::start`, which would otherwise
/// re-run DNS) only targets a vetted, already-resolved IP. This closes the
/// DNS-rebind
/// SSRF where a hostname resolves to a link-local cloud-metadata address.
///
/// We re-run `check_outbound` per resolved IP (formatting each as an
/// `IP:port` literal so the default policy's literal-IP link-local check
/// fires), reusing the public policy API rather than duplicating the
/// link-local range logic. A denial maps to the same IOException the
/// rest of the connect path uses.
fn resolve_and_vet(target: &str) -> Result<Vec<SocketAddr>, MethodCallFailed> {
    // First-pass policy check on the literal target — cheap, and rejects
    // a direct link-local IP before we even resolve.
    if let Err(reason) = crate::outbound_policy::check_outbound(target) {
        return Err(ioex(format!("connect denied by outbound policy: {reason}")));
    }

    // Fold IPv4-mapped destinations (`::ffff:a.b.c.d`) to plain IPv4 before
    // anything vets or dials them: on Windows an AF_INET6 socket cannot reach
    // one (WSAEADDRNOTAVAIL), and real JDK never produces such a destination
    // because `InetAddress` collapses the literal to an `Inet4Address`. See
    // `outbound_policy::normalize_connect_addr`.
    let addrs: Vec<SocketAddr> = match target.to_socket_addrs() {
        Ok(it) => it
            .map(crate::outbound_policy::normalize_connect_addr)
            .collect(),
        Err(e) => return Err(map_err(target, e)),
    };
    if addrs.is_empty() {
        return Err(ioex(format!("no addresses resolved for {target}")));
    }

    // Re-check the policy against every *resolved* address. A bracketed
    // literal keeps IPv6 `host:port` parsing unambiguous, matching the
    // policy's `host_part` expectations.
    for addr in &addrs {
        let literal = match addr {
            SocketAddr::V4(_) => format!("{}:{}", addr.ip(), addr.port()),
            SocketAddr::V6(_) => format!("[{}]:{}", addr.ip(), addr.port()),
        };
        if let Err(reason) = crate::outbound_policy::check_outbound(&literal) {
            return Err(ioex(format!(
                "connect denied by outbound policy: resolved address {} of {target} is blocked: {reason}",
                addr.ip()
            )));
        }
    }
    Ok(addrs)
}

/// Inner connect routine. When `allow_block` is true (blocking mode), we
/// wait for the connection to succeed/fail. In non-blocking mode we start a
/// real non-blocking OS connect and return false immediately (or true if the
/// OS completed it synchronously); `finishConnect()` later polls the live fd.
/// A wildcard address (0.0.0.0 / :: / the all-zeros IPv6 literal) is a
/// valid *bind* target ("all interfaces") but not a defined *connect*
/// destination — Windows rejects it outright with WSAEADDRNOTAVAIL (os
/// error 10049), which is exactly what a caller building its target via
/// `new InetSocketAddress(port)` (the single-int ctor, which produces a
/// wildcard address) hits. Real JDK's native connect path resolves this
/// to loopback before dialing (confirmed empirically: HotSpot connects
/// successfully to a wildcard-address target on this same Windows host).
/// Mirror that here rather than handing the OS a destination it was never
/// meant to receive.
fn connect_target_host(host: String) -> String {
    match host.as_str() {
        "0.0.0.0" => "127.0.0.1".to_string(),
        "::" | "0:0:0:0:0:0:0:0" => "::1".to_string(),
        _ => host,
    }
}

/// `SocketChannel.connect(UnixDomainSocketAddress)`.
///
/// A UDS connect is resolved entirely by the local kernel: it either succeeds
/// or is refused immediately, with no handshake to wait for. So there is no
/// "connection pending" state to model here — the JDK's own `connect0` for
/// AF_UNIX likewise returns 1 (completed) in every non-error case — and this
/// returns `true` unconditionally on success, in blocking and non-blocking
/// mode alike.
fn sc_connect_unix(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    path: &str,
) -> Result<bool, MethodCallFailed> {
    if !crate::uds::is_supported() {
        return Err(unsupported_uds());
    }
    if read_reg_id(ctx, this).is_some() {
        return Err(ioex("connect: channel is already connected or connecting"));
    }
    ipc_dbg(format!("connect unix path={path}"));
    uds_dbg(format_args!("connect path={path}"));
    // The connect() syscall itself can block briefly on a busy listen backlog,
    // and touches no Java heap — bracket it like the INET blocking path so a
    // concurrent stop-the-world pause never waits on this thread.
    ctx.begin_blocking_region();
    let connected = crate::uds::connect(path);
    ctx.end_blocking_region();
    let stream = connected.map_err(|e| map_err(path, e))?;

    let blocking = read_blocking_flag(ctx, this);
    stream
        .set_nonblocking(!blocking)
        .map_err(|e| map_err("set_nonblocking", e))?;
    let id = tcp_register(TcpHandle::Stream(Arc::new(stream)));
    tcp_blocking_state().write().insert(id, blocking);
    cf_set(ctx, this, F_REG_ID, Value::Int(id));
    cf_set(ctx, this, F_CONNECTED, Value::Int(1));
    cf_set(ctx, this, F_FAMILY, Value::Int(FAMILY_UNIX));
    let path_str = ctx.create_string(path);
    cf_set(ctx, this, F_UDS_PATH, Value::Object(Some(path_str)));
    Ok(true)
}

fn sc_connect_inner(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    sa: ObjectRef,
    allow_block: bool,
) -> Result<bool, MethodCallFailed> {
    if let Some(path) = decode_unix_socket_address(ctx, sa)? {
        return sc_connect_unix(ctx, this, &path);
    }
    let (host, port) = decode_socket_address(ctx, sa)?;
    let host = connect_target_host(host);
    let target = format!("{host}:{port}");
    ipc_dbg(format!("connect target={target} allow_block={allow_block}"));

    if let Some(id) = read_reg_id(ctx, this) {
        let bound = tcp_take_bound(id)
            .ok_or_else(|| ioex("connect: channel is already connected or connecting"))?;
        return sc_connect_bound(ctx, this, id, bound, &host, port, allow_block);
    }

    if allow_block {
        // Task #16: SSRF hardening. Route through `policy_connect`, which
        // (1) consults the outbound-host policy (default: reject link-local
        // cloud-metadata IPs like 169.254.169.254) and (2) applies the
        // configured connect timeout (default 30 s) so a black-hole target
        // can't pin the VM thread for the OS-default ~2 minutes.
        //
        // GC/STW-cooperation: `policy_connect` performs a genuine OS-level
        // blocking `connect()` (up to the configured timeout). This is the
        // path taken whenever a `SocketChannel` is used in its default
        // blocking mode (i.e. before `configureBlocking(false)` is called,
        // or via `sun.nio.ch.SocketAdaptor.connect()` — see
        // `sc_blocking_connect` above) — unlike the non-blocking branch
        // below, which never blocks the OS thread. Bracket it in
        // `begin_blocking_region`/`end_blocking_region` so a concurrent STW
        // pause (JIT takeover or GC) does not count this thread as an
        // expected cooperator and wait on it forever. Same pattern as
        // `socket_accept`/`socket_connect` in `plain_socket.rs`.
        ctx.begin_blocking_region();
        let connect_result = crate::outbound_policy::policy_connect(&target);
        ctx.end_blocking_region();
        let stream = match connect_result {
            Ok(s) => s,
            Err(crate::outbound_policy::PolicyConnectError::Denied(reason)) => {
                return Err(ioex(format!("connect denied by outbound policy: {reason}")));
            }
            Err(crate::outbound_policy::PolicyConnectError::Io(e)) => {
                return Err(map_err(&target, e));
            }
        };
        // Apply current blocking state if non-blocking flag was set before
        // connect (rare, but handled).
        let blocking = read_blocking_flag(ctx, this);
        if !blocking {
            stream
                .set_nonblocking(true)
                .map_err(|e| map_err("set_nonblocking", e))?;
        }
        let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
        let id = tcp_register(TcpHandle::Stream(Arc::new(stream)));
        tcp_blocking_state().write().insert(id, blocking);
        cf_set(ctx, this, F_REG_ID, Value::Int(id));
        cf_set(ctx, this, F_CONNECTED, Value::Int(1));
        cf_set(ctx, this, F_LOCAL_PORT, Value::Int(local_port));
        let host_str = ctx.create_string(&host);
        cf_set(ctx, this, F_REMOTE, Value::Object(Some(host_str)));
        cf_set(ctx, this, F_REMOTE_PORT, Value::Int(port as i32));
        ipc_dbg(format!(
            "connect success(blocking) id={id} local_port={local_port}"
        ));
        return Ok(true);
    }

    // Task #16 / H3b: policy gate also applies to non-blocking connects.
    // The blocking branch above routes through `policy_connect`, which
    // re-checks every *resolved* SocketAddr. The non-blocking branch must
    // do the same: a `check_outbound(&target)` on the literal `host:port`
    // string only blocks targets that *parse* as a link-local IP — it
    // does NOT resolve DNS. Since `nb_connect::start` below dials a concrete
    // SocketAddr, a hostname that resolves to `169.254.169.254` would slip
    // through (DNS-rebind SSRF). So we resolve here, vet every resolved
    // IP against the outbound policy, and dial the *vetted* SocketAddr(s)
    // directly — never re-resolving the original hostname downstream.
    let mut vetted = resolve_and_vet(&target)?;
    // Prefer IPv4 addresses first — this matches HotSpot's default resolution
    // order (`java.net.preferIPv4Stack` semantics) and, critically, the
    // address CratonVM's `InetAddress.getLoopbackAddress()` hands out for the
    // synthetic test servers (`127.0.0.1`). Re-resolving a hostname like
    // `"localhost"` yields BOTH `127.0.0.1` and `::1` in an unspecified order;
    // a non-blocking connect commits to a single family (a dead loopback
    // address does not refuse promptly on Windows, so we cannot cheaply probe
    // which family is live). Ordering IPv4 first makes the dial land on the
    // family the server actually bound. (Stable partition preserves the
    // resolver's relative order within each family.)
    vetted.sort_by_key(|a| if a.is_ipv4() { 0 } else { 1 });

    // Real non-blocking connect (ES-HANG-02 residual 1). Start a genuine
    // non-blocking OS connect on the first vetted address and register the
    // **live pollable socket** in the tcp_registry. `connect()` then returns
    // immediately — true if the OS completed it synchronously (warm loopback
    // on some platforms), false (in progress) otherwise. Because the
    // connecting socket is a real fd, the JDK selector reports `OP_CONNECT`
    // for it through the ordinary write-readiness path; `finishConnect()`
    // resolves it via `SO_ERROR`. This eliminates the prior background-pool
    // model whose fd-less `Connecting` entry caused the Apache-NIO reactor to
    // lose a request (CancelledKeyException) when its connect deadline fired
    // before the deferred channel was wired in — and it does so WITHOUT any
    // manual selector OP_CONNECT injection (which double-fired the connecting
    // reactor's session request → IllegalStateException).
    let mut pending: Option<TcpStream> = None;
    let mut deferred_failure: Option<(TcpStream, std::io::Error)> = None;
    let mut last_err: Option<std::io::Error> = None;
    for addr in &vetted {
        match crate::nb_connect::start(addr) {
            Ok(crate::nb_connect::StartConnect::Connected(stream)) => {
                let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
                let id = tcp_register(TcpHandle::Stream(Arc::new(stream)));
                tcp_blocking_state().write().insert(id, false);
                cf_set(ctx, this, F_REG_ID, Value::Int(id));
                cf_set(ctx, this, F_CONNECTED, Value::Int(1));
                cf_set(ctx, this, F_LOCAL_PORT, Value::Int(local_port));
                let host_str = ctx.create_string(&host);
                cf_set(ctx, this, F_REMOTE, Value::Object(Some(host_str)));
                cf_set(ctx, this, F_REMOTE_PORT, Value::Int(port as i32));
                ipc_dbg(format!(
                    "connect success(nonblocking-immediate) id={id} local_port={local_port}"
                ));
                return Ok(true);
            }
            Ok(crate::nb_connect::StartConnect::InProgress(stream)) => {
                // Remember the first in-progress socket but keep scanning the
                // remaining vetted addresses for one that completes instantly.
                if pending.is_none() {
                    pending = Some(stream);
                }
            }
            Ok(crate::nb_connect::StartConnect::DeferredFailure(stream, error)) => {
                if deferred_failure.is_none() {
                    deferred_failure = Some((stream, error));
                }
            }
            Err(e) => {
                ipc_dbg(format!("connect start failed addr={addr}: {e}"));
                last_err = Some(e);
            }
        }
    }

    if let Some((stream, error)) = deferred_failure {
        let id = tcp_register(TcpHandle::ConnectFailed(stream, error));
        tcp_blocking_state().write().insert(id, false);
        cf_set(ctx, this, F_REG_ID, Value::Int(id));
        // Do NOT mark this connected. `SocketChannel.connect()` returning
        // `true` is the JDK contract for "connected immediately" — Netty's
        // `AbstractNioChannel.AbstractNioUnsafe.connect()` treats a `true`
        // return as an unconditional success and fulfills the connect
        // promise right there, never checking SO_ERROR again. The channel
        // only discovers the refusal later, on its first write, as a raw
        // "Connection refused" instead of the typed `ConnectException`
        // real code (and Spring Boot's `PortInUseException`-style cause-chain
        // walks) expects from a failed connect.
        //
        // Returning `false` here is safe: `probe_connect_status` (below)
        // already treats a registered `ConnectFailed` entry as immediately
        // `Ready`, so the selector (`nio_selector.rs`, both the Windows
        // WSAPoll path and the Linux epoll path) delivers `OP_CONNECT`
        // readiness for it on the very next poll without ever blocking —
        // there is no "untouched socket the selector can't see" problem to
        // work around. The reactor then calls `finishConnect()`, which
        // already correctly reports the saved failure (`Verdict::Failed`
        // below) as a typed exception.
        let host_str = ctx.create_string(&host);
        cf_set(ctx, this, F_REMOTE, Value::Object(Some(host_str)));
        cf_set(ctx, this, F_REMOTE_PORT, Value::Int(port as i32));
        ipc_dbg(format!("connect deferred failure(nonblocking) id={id}"));
        return Ok(false);
    }

    if let Some(stream) = pending {
        let id = tcp_register(TcpHandle::Connecting(stream));
        tcp_blocking_state().write().insert(id, false);
        cf_set(ctx, this, F_REG_ID, Value::Int(id));
        let host_str = ctx.create_string(&host);
        cf_set(ctx, this, F_REMOTE, Value::Object(Some(host_str)));
        cf_set(ctx, this, F_REMOTE_PORT, Value::Int(port as i32));
        ipc_dbg(format!("connect pending(nonblocking) id={id}"));
        return Ok(false);
    }

    // Every vetted address failed synchronously (e.g. immediate
    // ECONNREFUSED on Linux loopback). Surface the error so the caller's
    // reactor fails the request fast and the RestClient retries another node.
    Err(map_err(
        &target,
        last_err.unwrap_or_else(|| {
            std::io::Error::new(
                ErrorKind::AddrNotAvailable,
                format!("no addresses resolved for {target}"),
            )
        }),
    ))
}

fn sc_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let sa = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(null_socket_address("connect")),
    };
    let blocking = read_blocking_flag(ctx, this);
    let ok = sc_connect_inner(ctx, this, sa, blocking)?;
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

/// `SocketChannelImpl.blockingConnect(SocketAddress, long nanos)` (package-private)
/// — what `sun.nio.ch.SocketAdaptor.connect()` delegates to (the object returned by
/// `SocketChannel.socket()`). Performs a blocking connect and returns void; the
/// nanos timeout is best-effort ignored (the blocking connect inner already waits).
/// Without this, Gradle's TcpOutgoingConnector (socketChannel.socket().connect())
/// NoSuchMethodErrors.
fn sc_blocking_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    let sa = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(null_socket_address("blockingConnect")),
    };
    let ok = sc_connect_inner(ctx, this, sa, true)?;
    if !ok {
        return Err(ioex("blockingConnect: connection refused"));
    }
    cf_set(ctx, this, F_CONNECTED, Value::Int(1));
    Ok(None)
}

fn sc_finish_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let id = match read_reg_id(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))),
    };

    // Probe the current state of the registry entry. For a Connecting socket
    // we poll the **real fd** for write/error readiness + SO_ERROR (no
    // background worker / completion channel any more).
    use crate::nb_connect::ConnectPoll;
    enum Verdict {
        Connected,    // already a Stream
        Pending,      // still connecting
        Promote(i32), // connecting socket completed → local port
        Failed(MethodCallFailed),
        NotConnecting,
    }
    let verdict = {
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Stream(_)) => Verdict::Connected,
            Some(TcpHandle::Connecting(s)) => match crate::nb_connect::poll(s) {
                ConnectPoll::Pending => Verdict::Pending,
                ConnectPoll::Connected => {
                    let lp = s.local_addr().map(|a| a.port() as i32).unwrap_or(0);
                    Verdict::Promote(lp)
                }
                ConnectPoll::Failed(e) => Verdict::Failed(map_err("finishConnect", e)),
            },
            Some(TcpHandle::ConnectFailed(_, error)) => Verdict::Failed(map_err(
                "finishConnect",
                std::io::Error::new(error.kind(), error.to_string()),
            )),
            _ => Verdict::NotConnecting,
        }
    };

    match verdict {
        Verdict::Connected => {
            cf_set(ctx, this, F_CONNECTED, Value::Int(1));
            Ok(Some(Value::Int(1)))
        }
        Verdict::Pending => Ok(Some(Value::Int(0))),
        Verdict::Promote(local_port) => {
            // Promote Connecting -> Stream in place (the fd is unchanged; we
            // just reclassify it now that the OS reports the connect done).
            let mut map = tcp_registry().write();
            match map.remove(&id) {
                Some(TcpHandle::Connecting(stream)) => {
                    map.insert(id, TcpHandle::Stream(Arc::new(stream)));
                    drop(map);
                    cf_set(ctx, this, F_CONNECTED, Value::Int(1));
                    cf_set(ctx, this, F_LOCAL_PORT, Value::Int(local_port));
                    Ok(Some(Value::Int(1)))
                }
                other => {
                    // Lost a race with close()/another finishConnect — restore.
                    if let Some(h) = other {
                        map.insert(id, h);
                    }
                    Ok(Some(Value::Int(0)))
                }
            }
        }
        Verdict::Failed(e) => {
            tcp_remove(id);
            Err(e)
        }
        Verdict::NotConnecting => Err(ioex("finishConnect: socket not in connecting state")),
    }
}

// ---------------------------------------------------------------------------
// SocketChannel.read / write
// ---------------------------------------------------------------------------

/// Read up to `len` bytes from a non-blocking stream. Returns:
///   * Ok(Some(n)) on success (n bytes)
///   * Ok(None) when EAGAIN/WouldBlock
///   * Ok(Some(-1)) on EOF
///   * Err(...) on hard error
/// FNV-1a 64-bit hash, used only by the `CRATONVM_DBG_SC_READ` diagnostic
/// below to cheaply fingerprint the bytes a given `sc_read` call actually
/// delivered, so two calls can be compared for byte-identical content
/// without dumping full hex payloads into the log.
fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// What one of the I/O natives should operate on, resolved with the registry
/// lock held only for the lookup itself.
///
/// See `TcpHandle::Stream`: the syscall must run with the lock released, so
/// the live socket is handed back as a cheap `Arc` clone rather than a borrow
/// into the map.
enum StreamTarget {
    /// The live connection; the registry lock is no longer held.
    Ready(Arc<TcpStream>),
    /// A non-blocking connect is still in flight — report "no progress".
    Connecting,
    /// The connect already failed — surface it as the operation's error.
    Failed(std::io::Error),
    /// Closed, a listener, or an unknown id.
    Unavailable,
}

fn resolve_stream(id: i32) -> StreamTarget {
    let map = tcp_registry().read();
    match map.get(&id) {
        Some(TcpHandle::Stream(s)) => StreamTarget::Ready(Arc::clone(s)),
        Some(TcpHandle::Connecting(_)) => StreamTarget::Connecting,
        Some(TcpHandle::ConnectFailed(_, error)) => {
            StreamTarget::Failed(std::io::Error::new(error.kind(), error.to_string()))
        }
        _ => StreamTarget::Unavailable,
    }
}

fn try_read_nb(stream: &TcpStream, buf: &mut [u8]) -> Result<Option<i32>, std::io::Error> {
    let mut s = stream;
    loop {
        match s.read(buf) {
            Ok(0) => return Ok(Some(-1)),
            Ok(n) => return Ok(Some(n as i32)),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                return Ok(None)
            }
            // EINTR consumes no bytes. Retrying is required instead of
            // reporting a short/zero channel operation or aborting a Tomcat
            // response. Some Linux wrappers preserve it only as raw errno 4.
            Err(e) if e.kind() == ErrorKind::Interrupted || e.raw_os_error() == Some(4) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// How long a blocking channel read parks inside one poll before re-asking the
/// registry whether the channel was closed under it.
///
/// A liveness bound, not a latency cost: the poll returns the instant the
/// socket becomes readable, so payload is never delayed by it. Same role as
/// [`ACCEPT_CLOSE_POLL`] on the accept side.
const READ_CLOSE_POLL_MS: i32 = 25;

/// Is `id` still a live stream? [`sc_close`] calls `tcp_remove`, so this flips
/// exactly when Java closed the channel.
fn stream_still_registered(id: i32) -> bool {
    matches!(tcp_registry().read().get(&id), Some(TcpHandle::Stream(_)))
}

/// The error a parked read reports once its channel is closed from another
/// thread.
///
/// `ErrorKind::Interrupted` is unambiguous at this site: [`try_read_nb`]
/// reissues every real EINTR, and `net::poll_stream_readable` reports one as
/// "not ready" rather than as an error. So [`closed_or_io_error`] can turn
/// exactly this into `AsynchronousCloseException`.
fn channel_async_closed_err() -> std::io::Error {
    std::io::Error::new(ErrorKind::Interrupted, "channel closed asynchronously")
}

/// A blocking channel read that observes an asynchronous `close()`.
///
/// # Why the plain blocking read could not
///
/// A blocking-mode channel leaves its `TcpStream` in genuine OS-blocking mode,
/// so [`try_read_nb`] parks inside `recv`. Nothing `sc_close` does reaches that
/// park: `lingering_channel_close` issues `shutdown(Write)` only — deliberately
/// so, see its comment on RST-prone `Shutdown::Both` — and `tcp_remove` merely
/// drops the map's `Arc`, which cannot close the OS handle while this reader
/// holds a clone of it. So the reader stayed in `recv` until the peer sent
/// something or the process exited, which is `RJdkNio.selectorAndAsyncClose`
/// reporting "the blocked reader never woke up".
///
/// HotSpot breaks the same park by closing the descriptor underneath it
/// (`closesocket` on Windows, `dup2` of a pre-closed fd plus a signal on Unix).
/// Neither is expressible over an `Arc<TcpStream>` without closing a handle
/// another thread is mid-syscall on. So park in `poll` instead of in `recv`,
/// and re-ask the registry every [`READ_CLOSE_POLL_MS`] — the close-aware shape
/// [`accept_close_aware`] already uses, and which [`sc_blocking_read`] gets for
/// free by re-`resolve_stream`ing on every pass.
///
/// The registry lock is taken per pass and never held across the poll.
fn read_close_aware(
    id: i32,
    stream: &TcpStream,
    buf: &mut [u8],
) -> Result<Option<i32>, std::io::Error> {
    loop {
        let ready = match crate::net::poll_stream_readable(stream, READ_CLOSE_POLL_MS) {
            Some(result) => result?,
            // No poll primitive on this target: the pre-2026-08-07 blocking
            // read, which cannot see the close but at least still transfers.
            None => return try_read_nb(stream, buf),
        };
        // Asked AFTER the poll so a close landing while we are parked is seen
        // on the next pass, and a close racing a readiness edge still wins —
        // completing a read on a channel Java has closed is precisely what
        // `AsynchronousCloseException` exists to prevent.
        if !stream_still_registered(id) {
            return Err(channel_async_closed_err());
        }
        if !ready {
            continue;
        }
        match try_read_nb(stream, buf) {
            // Readable, then not: a concurrent reader on this channel took the
            // bytes. Park again — a blocking read must not answer 0.
            Ok(None) => continue,
            other => return other,
        }
    }
}

fn try_write_nb(stream: &TcpStream, data: &[u8]) -> Result<Option<i32>, std::io::Error> {
    let mut s = stream;
    loop {
        match s.write(data) {
            Ok(n) => return Ok(Some(n as i32)),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(None),
            // Like read(), a signal interruption has not written any bytes;
            // retry so a header/body gathering write cannot be abandoned.
            Err(e) if e.kind() == ErrorKind::Interrupted || e.raw_os_error() == Some(4) => continue,
            Err(e) => return Err(e),
        }
    }
}

fn sc_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("read: null channel")),
    };
    let bb = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(ioex("read: null ByteBuffer")),
    };
    // Bound to a local first so no borrow of `ctx` outlives the lookup: the
    // closed-channel arm below needs `ctx` mutably.
    let reg_id = read_reg_id(ctx, this);
    let Some(id) = reg_id else {
        // `sc_close` wipes the synthetic state, so `F_OPEN` reading back 0 with
        // no registry id means "this channel was closed" — which
        // `java.nio.channels` spells `ClosedChannelException`, not the bare
        // IOException this used to answer. `RJdkNio`'s
        // `catch (ClosedChannelException)` walked straight past that one.
        return if cf_get(ctx, this, F_OPEN).as_int().unwrap_or(0) == 0 {
            Err(closed_channel_exception(ctx))
        } else {
            Err(ioex("read: channel not connected"))
        };
    };

    // Determine the writable region. We materialize into a heap buffer here
    // and copy into the buffer slot afterwards so we don't hold a registry
    // lock across `set_array_element`.
    let access =
        buffer_access(ctx, bb).ok_or_else(|| ioex("read: ByteBuffer has no decodable layout"))?;
    let len = match access {
        BufferAccess::Direct { length, .. } => length,
        BufferAccess::Heap { length, .. } => length,
    };
    if len <= 0 {
        return Ok(Some(Value::Int(0)));
    }
    let mut buf = vec![0u8; len as usize];
    // The OS read below may enter a GC-blocking region. Keep the Java buffer
    // rooted and reload it before writing the received bytes back.
    let bb_pin = ctx.pin_native_root(bb);

    // GC/STW-cooperation: when the channel is in its default *blocking*
    // mode (`configureBlocking(false)` never called — see F_BLOCKING /
    // `sc_connect_inner`'s `allow_block` branch), the underlying
    // `TcpStream` is left in genuine OS-blocking mode too, so `s.read()`
    // inside `try_read_nb` below can block indefinitely for data rather
    // than returning EAGAIN. Bracket the call in
    // `begin_blocking_region`/`end_blocking_region` unconditionally (cheap
    // for the common non-blocking case, where the call returns immediately)
    // so a concurrent STW pause never waits on a thread parked here. Same
    // pattern as `re1_socket_read_stream` in `net_phase_e.rs`.
    let blocking = read_blocking_flag(ctx, this);
    ctx.begin_blocking_region();
    let read_result = match resolve_stream(id) {
        StreamTarget::Ready(s) => {
            // A blocking channel leaves the OS socket blocking, so a bare
            // `try_read_nb` parks inside `recv`, where no `close()` on another
            // thread can reach it. See `read_close_aware`.
            let r = if blocking {
                read_close_aware(id, &s, &mut buf)
            } else {
                try_read_nb(&s, &mut buf)
            };
            ctx.end_blocking_region();
            r
        }
        StreamTarget::Connecting => {
            ctx.end_blocking_region();
            ctx.unpin_native_roots(bb_pin);
            return Ok(Some(Value::Int(0)));
        }
        StreamTarget::Failed(error) => {
            ctx.end_blocking_region();
            ctx.unpin_native_roots(bb_pin);
            return Err(map_err("read", error));
        }
        StreamTarget::Unavailable => {
            ctx.end_blocking_region();
            ctx.unpin_native_roots(bb_pin);
            return Err(ioex("read: channel not a stream"));
        }
    };

    let n_opt = match read_result {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(bb_pin);
            return Err(closed_or_io_error(ctx, "read", e));
        }
    };
    let n = match n_opt {
        Some(v) => v,
        None => {
            ctx.unpin_native_roots(bb_pin);
            return Ok(Some(Value::Int(0)));
        }
    };
    if n > 0 {
        crate::net::socket_capture('r', id, &buf[..n as usize]);
        // Reload `bb` through the pin BEFORE touching it again: the blocking
        // read above may have crossed a GC pause that relocated the object,
        // so the original `bb` reference could be stale here (see
        // `pin_native_root`'s doc comment on this exact hazard). Both the
        // diagnostic below and the real write path use this reloaded ref.
        let bb = ctx.read_native_pin(bb_pin, bb);
        // Diagnostic (CRATONVM_DBG_SC_READ=1, added 2026-07-17 continuing the
        // StompWebSocketIntegrationTests premature-close investigation): the
        // prior session pinned the failure to the server dispatching one
        // client-written STOMP CONNECT frame to Spring's
        // handleMessageFromClient TWICE, with live gdb confirming exactly 2
        // physical `sc_read` calls occur before either dispatch (Jetty
        // backend) — i.e. this native genuinely gets invoked twice, each
        // apparently returning a real, non-empty payload. Fingerprint every
        // real (n>0) read with an FNV-1a hash + byte count + the buffer's
        // `position` field before/after, so a rerun can show directly
        // whether the two reads return byte-identical content (a
        // duplicate-delivery bug below `try_read_nb`/the OS socket) or two
        // genuinely different byte ranges (pointing the remaining
        // investigation at Jetty's/Tomcat's own frame-parser instead). Kept
        // as a permanent opt-in hook, zero cost when unset, matching
        // CRATONVM_DBG_SC_CLOSE's precedent in this same file.
        if io_flags().dbg_sc_read {
            let pos_before = match ctx.get_field_by_name(bb, "position") {
                Value::Int(v) => v,
                _ => -1,
            };
            let (local, peer) = match tcp_registry().read().get(&id) {
                Some(TcpHandle::Stream(s)) => (
                    s.local_addr().map(|a| a.to_string()).unwrap_or_default(),
                    s.peer_addr().map(|a| a.to_string()).unwrap_or_default(),
                ),
                _ => (String::new(), String::new()),
            };
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let hash = fnv1a64(&buf[..n as usize]);
            let dump_len = (n as usize).min(64);
            let hex: String = buf[..dump_len].iter().map(|b| format!("{b:02x}")).collect();
            eprintln!(
                "[SC_READ] t={ms} id={id:#x} local={local} peer={peer} n={n} pos_before={pos_before} fnv1a={hash:#018x} hex[0..{dump_len}]={hex}"
            );
            // 2026-07-17 continuation: a live rerun of StompWebSocketIntegrationTests
            // against this diagnostic found the Tomcat parameterization's `sc_read`
            // returning the SAME (id, byte-content) pair dozens of times in a row
            // (identical FNV-1a hash) at a steady ~20-60ms cadence -- i.e. the
            // native read layer itself, not just Spring's message dispatch, sees
            // byte-identical "new" reads. Capture ONE Java stack trace the first
            // time a read's hash repeats the immediately preceding read on the
            // same channel id, to pin the exact Tomcat call site re-issuing the
            // read (only once per repeat streak, to avoid flooding the log across
            // a long redelivery spin).
            fn last_read_hash() -> &'static parking_lot::Mutex<HashMap<i32, (u64, bool)>> {
                static T: OnceLock<parking_lot::Mutex<HashMap<i32, (u64, bool)>>> = OnceLock::new();
                T.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
            }
            let mut streak_started = false;
            {
                let mut m = last_read_hash().lock();
                let entry = m.entry(id).or_insert((0, false));
                if entry.0 == hash && !entry.1 {
                    entry.1 = true;
                    streak_started = true;
                } else if entry.0 != hash {
                    *entry = (hash, false);
                }
            }
            if streak_started {
                let raw_trace = ctx.capture_stack_trace(0);
                eprintln!(
                    "[SC_READ_REPEAT_STACK] t={ms} id={id:#x} n={n} fnv1a={hash:#018x} ({} frames)",
                    raw_trace.len()
                );
                for entry in raw_trace.iter().rev() {
                    let file = entry.source_file.as_deref().unwrap_or("?");
                    eprintln!(
                        "  at {}.{}({}:{})",
                        entry.class_name, entry.method_name, file, entry.line_number
                    );
                }
            }
        }
        let written = buffer_write_bytes(ctx, bb, &buf[..n as usize]);
        buffer_advance(ctx, bb, written);
    }
    ctx.unpin_native_roots(bb_pin);
    Ok(Some(Value::Int(n)))
}

fn sc_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("write: null channel")),
    };
    let bb = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(ioex("write: null ByteBuffer")),
    };
    let id = read_reg_id(ctx, this).ok_or_else(|| ioex("write: channel not connected"))?;
    let data = buffer_read_bytes(ctx, bb).unwrap_or_default();
    if io_flags().dbg_sc_write {
        let position = ctx.get_field_by_name(bb, "position");
        let limit = ctx.get_field_by_name(bb, "limit");
        let address = ctx.get_field_by_name(bb, "address");
        eprintln!(
            "[SC_WRITE] id={id:#x} class={} position={position:?} limit={limit:?} address={address:?} data_len={}",
            ctx.class_name_of_id(ctx.class_id_of_object(bb)).unwrap_or_default(),
            data.len(),
        );
    }
    if data.is_empty() {
        return Ok(Some(Value::Int(0)));
    }
    // The OS write below may enter a GC-blocking region. Keep the Java buffer
    // rooted until its position has been advanced after the write completes.
    let bb_pin = ctx.pin_native_root(bb);
    // GC/STW-cooperation: same reasoning as `sc_read` above — a
    // blocking-mode channel's `TcpStream` can genuinely block in
    // `try_write_nb`'s `s.write()` (e.g. a full socket send buffer with a
    // slow/stalled peer), so bracket it unconditionally.
    ctx.begin_blocking_region();
    let write_result = match resolve_stream(id) {
        StreamTarget::Ready(s) => {
            let r = try_write_nb(&s, &data).map_err(|e| map_err("write", e));
            ctx.end_blocking_region();
            r
        }
        StreamTarget::Connecting => {
            ctx.end_blocking_region();
            ctx.unpin_native_roots(bb_pin);
            return Ok(Some(Value::Int(0)));
        }
        StreamTarget::Failed(error) => {
            ctx.end_blocking_region();
            ctx.unpin_native_roots(bb_pin);
            return Err(map_err("write", error));
        }
        StreamTarget::Unavailable => {
            ctx.end_blocking_region();
            ctx.unpin_native_roots(bb_pin);
            return Err(ioex("write: channel not a stream"));
        }
    };

    let n_opt = match write_result {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(bb_pin);
            return Err(e);
        }
    };
    let n = match n_opt {
        Some(v) => v,
        None => {
            ctx.unpin_native_roots(bb_pin);
            return Ok(Some(Value::Int(0)));
        }
    };
    if n > 0 {
        crate::net::socket_capture('w', id, &data[..n as usize]);
        if io_flags().dbg_sc_write {
            eprintln!("[SC_WRITE] id={id:#x} wrote={n}");
        }
        let bb = ctx.read_native_pin(bb_pin, bb);
        buffer_advance(ctx, bb, n);
    }
    ctx.unpin_native_roots(bb_pin);
    Ok(Some(Value::Int(n)))
}

// ---------------------------------------------------------------------------
// SocketChannel — vectored (gathering / scattering) I/O  [DF03]
// ---------------------------------------------------------------------------
//
// `java.nio.channels.SocketChannel` declares the gathering `write(ByteBuffer[],
// int, int)` and scattering `read(ByteBuffer[], int, int)` as ABSTRACT (it
// inherits them from GatheringByteChannel / ScatteringByteChannel); the
// concrete bodies live in `sun.nio.ch.SocketChannelImpl`. CratonVM's channel
// objects are synthetic `java/nio/channels/SocketChannel` instances, so a
// direct call to the three-arg form resolves to the abstract declaration (no
// Code attribute) → AbstractMethodError. Tomcat's NIO write path uses the
// gathering form for header+body flushes, so the websocket connector hit this
// (DF03). We implement both by looping over the buffer slice and reusing the
// same `TcpStream` + ByteBuffer plumbing as the scalar read/write natives.
//
// The single-arg final overloads `read(ByteBuffer[])` / `write(ByteBuffer[])`
// delegate to these in the real JDK; we register the same handlers for them too
// (detecting the 2-arg arity → offset 0, length = array length) so dispatch is
// robust regardless of whether the final bytecode body is taken.

/// Resolve the `(offset, length)` window for a vectored op, tolerating both the
/// 3-arg `(srcs, offset, length)` and the convenience `(srcs)` arities, and
/// clamping the result to a valid sub-range of an `arr_len`-element array.
fn vec_window(args: &[Value], arr_len: i32) -> (i32, i32) {
    let (offset, length) = if args.len() >= 4 {
        (int_arg(args, 2), int_arg(args, 3))
    } else {
        (0, arr_len)
    };
    let start = offset.clamp(0, arr_len);
    let end = offset.saturating_add(length).clamp(start, arr_len);
    (start, end)
}

/// Gathering write: `write(ByteBuffer[] srcs, int offset, int length)` → long
/// (and the `write(ByteBuffer[])` convenience form). Concatenates the readable
/// region of each buffer in the slice, performs one non-blocking write, then
/// advances each buffer's position by the number of its own bytes that were
/// actually sent. Returns the total bytes written (0 on EAGAIN).
fn sc_write_gathering(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("write(gathering): null channel")),
    };
    let srcs = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(ioex("write(gathering): null buffer array")),
    };
    let id =
        read_reg_id(ctx, this).ok_or_else(|| ioex("write(gathering): channel not connected"))?;

    // Collect each buffer's readable region (in order), keeping the buffer ref
    // so we can advance its position by the bytes actually consumed.
    let arr_len = ctx.array_length(srcs) as i32;
    let (start, end) = vec_window(args, arr_len);
    let mut chunks = Vec::new();
    let mut total: usize = 0;
    for i in start..end {
        if let Value::Object(Some(bb)) = ctx.get_array_element(srcs, i as usize) {
            let bytes = buffer_read_bytes(ctx, bb).unwrap_or_default();
            total += bytes.len();
            let pin = ctx.pin_native_root(bb);
            chunks.push((pin, bb, bytes));
        }
    }
    if total == 0 {
        ipc_dbg(format!(
            "write(gathering) empty id={id} buffers={} window={start}..{end}",
            arr_len
        ));
        for (pin, _, _) in chunks {
            ctx.unpin_native_roots(pin);
        }
        return Ok(Some(Value::Long(0)));
    }
    let mut data = Vec::with_capacity(total);
    for (_, _, bytes) in &chunks {
        data.extend_from_slice(bytes);
    }

    // A gathering write can block for exactly the same reason as a scalar
    // SocketChannel.write. The copied payload and every Java source buffer are
    // rooted above, so make this a GC-cooperative blocking region as well.
    // Without this bracket, a full send buffer can leave a mutator in native
    // I/O while a concurrent moving collection waits for it to reach a
    // safepoint; that is the remaining transport-pressure hole in this path.
    ctx.begin_blocking_region();
    let write_result = match resolve_stream(id) {
        StreamTarget::Ready(s) => {
            let r = try_write_nb(&s, &data).map_err(|e| map_err("write(gathering)", e));
            ctx.end_blocking_region();
            r
        }
        StreamTarget::Connecting => {
            ctx.end_blocking_region();
            for (pin, _, _) in &chunks {
                ctx.unpin_native_roots(*pin);
            }
            return Ok(Some(Value::Long(0)));
        }
        StreamTarget::Failed(error) => {
            ctx.end_blocking_region();
            for (pin, _, _) in &chunks {
                ctx.unpin_native_roots(*pin);
            }
            return Err(map_err("write(gathering)", error));
        }
        StreamTarget::Unavailable => {
            ctx.end_blocking_region();
            for (pin, _, _) in &chunks {
                ctx.unpin_native_roots(*pin);
            }
            return Err(ioex("write(gathering): channel not a stream"));
        }
    };
    let n_opt = match write_result {
        Ok(v) => v,
        Err(e) => {
            for (pin, _, _) in &chunks {
                ctx.unpin_native_roots(*pin);
            }
            return Err(e);
        }
    };
    let n = match n_opt {
        Some(v) => v,
        None => {
            for (pin, _, _) in &chunks {
                ctx.unpin_native_roots(*pin);
            }
            return Ok(Some(Value::Long(0)));
        }
    };
    if n > 0 {
        crate::net::socket_capture('w', id, &data[..n as usize]);
        // Distribute the written count across the source buffers, advancing
        // each position by the portion of its bytes that made it out.
        let mut remaining = n;
        for (pin, bb, bytes) in &chunks {
            if remaining <= 0 {
                break;
            }
            let consume = (bytes.len() as i32).min(remaining);
            let bb = ctx.read_native_pin(*pin, *bb);
            buffer_advance(ctx, bb, consume);
            remaining -= consume;
        }
    }
    for (pin, _, _) in chunks {
        ctx.unpin_native_roots(pin);
    }
    Ok(Some(Value::Long(n as i64)))
}

/// Scattering read: `read(ByteBuffer[] dsts, int offset, int length)` → long
/// (and the `read(ByteBuffer[])` convenience form). Reads up to the combined
/// writable capacity of the slice in one non-blocking call, then scatters the
/// bytes into the destination buffers in order. Returns the total bytes read,
/// 0 on EAGAIN, or -1 on EOF.
fn sc_read_scattering(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("read(scattering): null channel")),
    };
    let dsts = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(ioex("read(scattering): null buffer array")),
    };
    let reg_id = read_reg_id(ctx, this);
    let Some(id) = reg_id else {
        // See `sc_read`: no registry id + `F_OPEN == 0` is a closed channel.
        return if cf_get(ctx, this, F_OPEN).as_int().unwrap_or(0) == 0 {
            Err(closed_channel_exception(ctx))
        } else {
            Err(ioex("read(scattering): channel not connected"))
        };
    };

    // Sum the writable capacity across the buffer slice; remember each target
    // so we can scatter the bytes back afterward (in array order).
    let arr_len = ctx.array_length(dsts) as i32;
    let (start, end) = vec_window(args, arr_len);
    let mut targets: Vec<ObjectRef> = Vec::new();
    let mut total: i64 = 0;
    for i in start..end {
        if let Value::Object(Some(bb)) = ctx.get_array_element(dsts, i as usize) {
            let room = match buffer_access(ctx, bb) {
                Some(BufferAccess::Direct { length, .. }) => length,
                Some(BufferAccess::Heap { length, .. }) => length,
                None => 0,
            };
            if room > 0 {
                total += room as i64;
                targets.push(bb);
            }
        }
    }
    if total <= 0 {
        return Ok(Some(Value::Long(0)));
    }
    let cap = total.min(i32::MAX as i64) as usize;
    let mut buf = vec![0u8; cap];

    // AUDIT 2026-07-26 (native-io-audit): this path used to call `try_read_nb`
    // with NO `begin_blocking_region`/`end_blocking_region` bracket, unlike
    // every sibling (`sc_read` 1799, `sc_write` 1967, `sc_write_gathering`
    // 2109). A blocking-mode SocketChannel leaves its `TcpStream` in genuine
    // OS-blocking mode, so a scattering `read(ByteBuffer[])` — the shape
    // Jetty/Netty-style reactors use for header+body reads — parks in the
    // kernel indefinitely. A concurrent stop-the-world pause then waits
    // forever on a mutator that never reaches a safepoint: a whole-VM hang
    // with no exception and no diagnostic. This is the documented
    // "STW blocking-region missing native I/O family" issue class.
    //
    // The destination buffers are used after the region, so they are pinned
    // and reloaded through `read_native_pin` (a pause inside the region may
    // relocate them) — same protocol as `sc_write_gathering`.
    let pins: Vec<_> = targets.iter().map(|bb| ctx.pin_native_root(*bb)).collect();
    let blocking = read_blocking_flag(ctx, this);
    ctx.begin_blocking_region();
    let read_result = match resolve_stream(id) {
        StreamTarget::Ready(s) => {
            // Same asynchronous-close hazard as `sc_read` — see
            // `read_close_aware`. This is the shape Jetty/Netty-style reactors
            // use for header+body reads, so it parks just as long.
            let r = if blocking {
                read_close_aware(id, &s, &mut buf)
            } else {
                try_read_nb(&s, &mut buf)
            };
            ctx.end_blocking_region();
            r
        }
        StreamTarget::Connecting => {
            ctx.end_blocking_region();
            for pin in pins {
                ctx.unpin_native_roots(pin);
            }
            return Ok(Some(Value::Long(0)));
        }
        StreamTarget::Failed(error) => {
            ctx.end_blocking_region();
            for pin in pins {
                ctx.unpin_native_roots(pin);
            }
            return Err(map_err("read(scattering)", error));
        }
        StreamTarget::Unavailable => {
            ctx.end_blocking_region();
            for pin in pins {
                ctx.unpin_native_roots(pin);
            }
            return Err(ioex("read(scattering): channel not a stream"));
        }
    };
    let n_opt = match read_result {
        Ok(v) => v,
        Err(e) => {
            for pin in pins {
                ctx.unpin_native_roots(pin);
            }
            return Err(closed_or_io_error(ctx, "read(scattering)", e));
        }
    };
    let n = match n_opt {
        Some(v) => v,
        None => {
            for pin in pins {
                ctx.unpin_native_roots(pin);
            }
            return Ok(Some(Value::Long(0))); // EAGAIN
        }
    };
    if n < 0 {
        for pin in pins {
            ctx.unpin_native_roots(pin);
        }
        return Ok(Some(Value::Long(-1))); // EOF
    }
    if n > 0 {
        crate::net::socket_capture('r', id, &buf[..n as usize]);
        // Scatter the bytes into the destination buffers in order; each call
        // fills one buffer up to its remaining room, then we move to the next.
        let mut consumed = 0usize;
        for (pin, bb) in pins.iter().zip(targets.iter()) {
            if consumed >= n as usize {
                break;
            }
            let bb = ctx.read_native_pin(*pin, *bb);
            let written = buffer_write_bytes(ctx, bb, &buf[consumed..n as usize]);
            if written <= 0 {
                break;
            }
            buffer_advance(ctx, bb, written);
            consumed += written as usize;
        }
    }
    for pin in pins {
        ctx.unpin_native_roots(pin);
    }
    Ok(Some(Value::Long(n as i64)))
}

// ---------------------------------------------------------------------------
// SocketChannel — read-availability (FIONREAD), shared with sun/nio/ch/Net
// ---------------------------------------------------------------------------

/// Number of bytes readable without blocking on the channel backed by registry
/// `id`, or `None` when the id is not a live stream. Lets `sun/nio/ch/Net`'s
/// `available` native (net.rs) cover a SocketChannel-backed fd should one ever
/// reach it; the primary path is the `net_sockets` registry.
pub(crate) fn tcp_stream_available(id: i32) -> Option<i32> {
    let map = tcp_registry().read();
    match map.get(&id) {
        Some(TcpHandle::Stream(s)) => crate::net::socket_available_stream(s),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// SocketChannel — TCP options
// ---------------------------------------------------------------------------

fn apply_option(stream: &TcpStream, name: &str, val: i32) -> Result<(), std::io::Error> {
    match name {
        "TCP_NODELAY" => stream.set_nodelay(val != 0),
        "SO_KEEPALIVE" => Ok(()), // std::net offers no setter without socket2
        "SO_REUSEADDR" => Ok(()), // pre-bind only
        "SO_RCVBUF" | "SO_SNDBUF" => Ok(()),
        _ => Ok(()),
    }
}

fn read_option(stream: &TcpStream, name: &str) -> Result<i32, std::io::Error> {
    match name {
        "TCP_NODELAY" => Ok(if stream.nodelay()? { 1 } else { 0 }),
        // Keep an intentionally conservative non-zero fallback for channels
        // whose options are inspected before Java has set them. A zero buffer
        // size is not a valid Socket API result and causes NIO frameworks to
        // allocate zero-capacity codec buffers.
        "SO_RCVBUF" | "SO_SNDBUF" => Ok(64 * 1024),
        _ => Ok(0),
    }
}

/// `SocketChannel.setOption` is erased to `(SocketOption, Object)`, so real
/// JDK callers provide a boxed Integer or Boolean rather than a raw int.
fn socket_option_value(ctx: &mut dyn NativeContext, value: Value) -> i32 {
    match value {
        Value::Int(v) => v,
        Value::Object(Some(object)) => {
            let class_name = ctx
                .class_name_of_id(ctx.class_id_of_object(object))
                .unwrap_or_default();
            let (method, descriptor) = if class_name == "java/lang/Boolean" {
                ("booleanValue", "()Z")
            } else {
                ("intValue", "()I")
            };
            match ctx.invoke_virtual(object, method, descriptor, &[]) {
                Ok(Some(Value::Int(v))) => v,
                _ => 0,
            }
        }
        _ => 0,
    }
}

fn sc_set_option(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let opt_name = match obj_or_none(args, 1) {
        Some(o) => match ctx.get_field_by_name(o, "name") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => ctx.read_string(o).unwrap_or_default(),
        },
        None => String::new(),
    };
    // Accept either Int or Boolean payloads — both arrive as Value::Int here.
    let val = socket_option_value(ctx, args.get(2).copied().unwrap_or(Value::Int(0)));

    if let Some(id) = read_reg_id(ctx, this) {
        tcp_option_state()
            .write()
            .insert((id, opt_name.clone()), val);
        let map = tcp_registry().read();
        let stream: Option<&TcpStream> = match map.get(&id) {
            Some(TcpHandle::Stream(s)) => Some(s),
            Some(TcpHandle::Bound(s)) => Some(s),
            _ => None,
        };
        if let Some(s) = stream {
            if let Err(e) = apply_option(s, &opt_name, val) {
                return Err(map_err(&format!("setOption({opt_name})"), e));
            }
        }
    }
    Ok(Some(Value::Object(Some(this))))
}

fn sc_get_option(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let opt_name = match obj_or_none(args, 1) {
        Some(o) => match ctx.get_field_by_name(o, "name") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => ctx.read_string(o).unwrap_or_default(),
        },
        None => String::new(),
    };
    // `SocketChannel.getOption` is declared `<T> T getOption(SocketOption<T>)`,
    // so the native MUST return a *boxed* object (Boolean/Integer), not a raw
    // `Value::Int` — a primitive returned for an object-typed method coerces to
    // null, and the `SocketAdaptor` getters then `((Boolean) ...).booleanValue()`
    // → NPE. Box by the option's value type.
    let raw = if let Some(id) = read_reg_id(ctx, this) {
        if let Some(value) = tcp_option_state()
            .read()
            .get(&(id, opt_name.clone()))
            .copied()
        {
            value
        } else {
            let map = tcp_registry().read();
            match map.get(&id) {
                Some(TcpHandle::Stream(s)) => read_option(s, &opt_name).unwrap_or(0),
                Some(TcpHandle::Bound(s)) => read_option(s, &opt_name).unwrap_or(0),
                _ => 0,
            }
        }
    } else {
        0
    };
    box_socket_option(ctx, &opt_name, raw)
}

/// Box a socket-option value as the JDK type the `SocketOption<T>` declares:
/// `Boolean` for the flag options, otherwise `Integer`.
fn box_socket_option(ctx: &mut dyn NativeContext, opt_name: &str, raw: i32) -> MethodCallResult {
    let is_bool = matches!(
        opt_name,
        "TCP_NODELAY"
            | "SO_KEEPALIVE"
            | "SO_REUSEADDR"
            | "SO_REUSEPORT"
            | "SO_BROADCAST"
            | "SO_OOBINLINE"
    );
    if is_bool {
        ctx.invoke(
            "java/lang/Boolean",
            "valueOf",
            "(Z)Ljava/lang/Boolean;",
            &[Value::Int(if raw != 0 { 1 } else { 0 })],
        )
    } else {
        ctx.invoke(
            "java/lang/Integer",
            "valueOf",
            "(I)Ljava/lang/Integer;",
            &[Value::Int(raw)],
        )
    }
}

/// Resolve `java.net.StandardSocketOptions.<FIELD>`'s static value (a real
/// `SocketOption<?>` singleton instance), or `None` if the field can't be
/// resolved (defensive — should not happen for a real boot class).
fn standard_socket_option(ctx: &mut dyn NativeContext, field_name: &str) -> Option<Value> {
    let cid = ctx
        .ensure_class_initialized("java/net/StandardSocketOptions")
        .ok()?;
    let idx = ctx.static_field_index_by_name(cid, field_name)?;
    Some(ctx.get_static_field(cid, idx))
}

/// `{Socket,ServerSocket}Channel.supportedOptions()` — must return a real,
/// non-null `Set<SocketOption<?>>`. Without a native override, dispatch falls
/// through to the abstract `NetworkChannel.supportedOptions()` declaration
/// (no Code attribute), throwing `AbstractMethodError`. That's an `Error`,
/// not an `Exception`, so a caller that only `catch (IOException |
/// RuntimeException)` around it — e.g. Apache HttpClient5's
/// `SingleCoreIOReactor.prepareSocket`, which checks
/// `channel.supportedOptions().contains(TCP_NODELAY)` before setting it —
/// does NOT catch it: the `AbstractMethodError` propagates uncaught out of
/// the calling thread, silently killing it. See
/// fixed-suite-bugs/spring/spring-web-flow-outputstreamwriter-close-corruption-FIXED.md
/// root cause #3 — this silently killed HttpClient5's I/O reactor worker
/// thread mid-connection-setup, before it ever reached `SocketChannel
/// .connect()`, hanging every request through
/// `HttpComponentsClientHttpConnector` forever with no visible exception
/// anywhere (an uncaught `Error` on a bare `Thread` with no
/// `UncaughtExceptionHandler` just terminates that thread silently).
///
/// Advertise the options this shim actually recognizes in `apply_option`/
/// `read_option` above: `TCP_NODELAY` (genuinely wired to
/// `TcpStream::set_nodelay`), plus `SO_KEEPALIVE`/`SO_REUSEADDR`/
/// `SO_RCVBUF`/`SO_SNDBUF`/`SO_LINGER` (accepted no-ops — `std::net
/// ::TcpStream` exposes no setter for the latter three without the
/// `socket2` crate). Listing a no-op option here changes nothing behaviorally
/// (callers that skip a `setOption` call when it's unlisted would otherwise
/// just silently skip it instead of silently no-op-ing it) — the real fix
/// is simply that this method must never throw.
fn supported_socket_options(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let names = [
        "SO_RCVBUF",
        "SO_SNDBUF",
        "SO_KEEPALIVE",
        "SO_REUSEADDR",
        "SO_LINGER",
        "TCP_NODELAY",
    ];
    let mut values = Vec::with_capacity(names.len());
    for name in names {
        if let Some(v) = standard_socket_option(ctx, name) {
            values.push(v);
        }
    }
    let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), values.len());
    for (i, v) in values.into_iter().enumerate() {
        ctx.set_array_element(arr, i, v);
    }
    ctx.invoke(
        "java/util/Set",
        "of",
        "([Ljava/lang/Object;)Ljava/util/Set;",
        &[Value::Object(Some(arr))],
    )
}

fn sc_supported_options(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    supported_socket_options(ctx)
}

/// Same set for the asynchronous channels — see `async_socket::
/// aio_supported_options`.
pub(crate) fn supported_socket_options_pub(ctx: &mut dyn NativeContext) -> MethodCallResult {
    supported_socket_options(ctx)
}

// ---------------------------------------------------------------------------
// ServerSocketChannel — open / bind / accept / close
// ---------------------------------------------------------------------------

fn ssc_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    ssc_open_family_value(ctx, 0)
}

/// Shared body of `ServerSocketChannel.open()` / `open(ProtocolFamily)`.
fn ssc_open_family_value(ctx: &mut dyn NativeContext, family: i32) -> MethodCallResult {
    let ch = alloc_obj(ctx, "java/nio/channels/ServerSocketChannel", N_FIELDS);
    let ch = init_channel_locks(ctx, ch);
    cf_set(ctx, ch, F_OPEN, Value::Int(1));
    cf_set(ctx, ch, F_BLOCKING, Value::Int(1));
    cf_set(ctx, ch, F_REG_ID, Value::Int(-1));
    cf_set(ctx, ch, F_CONNECTED, Value::Int(0));
    cf_set(ctx, ch, F_LOCAL_PORT, Value::Int(0));
    cf_set(ctx, ch, F_REMOTE, Value::Object(None));
    cf_set(ctx, ch, F_REMOTE_PORT, Value::Int(0));
    cf_set(ctx, ch, F_FAMILY, Value::Int(family));
    Ok(Some(Value::Object(Some(ch))))
}

/// `ServerSocketChannel.open(ProtocolFamily)` (JDK 16+). This is the entry
/// point Tomcat's `NioEndpoint.initServerSocket()` takes for a connector
/// configured with `unixDomainSocketPath`. Only the family is recorded; the
/// AF_UNIX socket itself is created by `bind()`, mirroring the INET path.
fn ssc_open_family(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let family = decode_protocol_family(ctx, args, 0);
    if family == FAMILY_UNIX && !crate::uds::is_supported() {
        return Err(unsupported_uds());
    }
    ssc_open_family_value(ctx, family)
}

/// `ServerSocketChannel.bind(UnixDomainSocketAddress, backlog)`.
fn ssc_bind_unix(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    path: &str,
    backlog: i32,
) -> MethodCallResult {
    if !crate::uds::is_supported() {
        return Err(unsupported_uds());
    }
    let listener = crate::uds::UdsListener::bind(path, backlog).map_err(|e| map_err(path, e))?;
    uds_dbg(format_args!(
        "bind path={path} backlog={backlog} raw={:#x}",
        listener.raw() as u64
    ));
    let blocking = read_blocking_flag(ctx, this);
    if !blocking {
        listener
            .set_nonblocking(true)
            .map_err(|e| map_err("set_nonblocking listener", e))?;
    }
    let id = tcp_register(TcpHandle::UnixListener(listener));
    tcp_blocking_state().write().insert(id, blocking);

    cf_set(ctx, this, F_REG_ID, Value::Int(id));
    cf_set(ctx, this, F_FAMILY, Value::Int(FAMILY_UNIX));
    // A Unix-domain listener has no port; `ssc_is_bound` special-cases the
    // family so leaving F_LOCAL_PORT at 0 is correct, and it keeps
    // `NioEndpoint.getLocalPort()` reporting -1 as it does on HotSpot.
    let path_str = ctx.create_string(path);
    cf_set(ctx, this, F_UDS_PATH, Value::Object(Some(path_str)));
    Ok(Some(Value::Object(Some(this))))
}

/// Read the already-resolved numeric IP out of an `InetSocketAddress`
/// (`getAddress().getHostAddress()`), or `None` when the address is
/// unresolved / wildcard.
fn sa_resolved_ip(ctx: &mut dyn NativeContext, sa: ObjectRef) -> Option<String> {
    let ia = match ctx.invoke_virtual(sa, "getAddress", "()Ljava/net/InetAddress;", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return None,
    };
    let text = match ctx.invoke_virtual(ia, "getHostAddress", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s)?,
        _ => return None,
    };
    // Only trust it if it really is a literal — a shim that echoes the
    // hostname back would otherwise defeat the point.
    if text.parse::<std::net::IpAddr>().is_ok() {
        Some(text)
    } else {
        None
    }
}

/// Resolve a listen target to exactly ONE socket address.
///
/// `TcpListener::bind(&str)` walks *every* address the target resolves to and
/// binds the first that succeeds. For a name like `localhost`, which resolves
/// to both `::1` and `127.0.0.1`, that quietly converts "this port is already
/// taken" into "bound to the other family's loopback instead" — where the JDK
/// binds the single `InetAddress` held by the `InetSocketAddress` and raises
/// `BindException` on the second attempt.
///
/// That difference is load-bearing. Tomcat Tribes' `ReceiverBase` auto-bind
/// loop discovers its listen port purely by catching that `BindException` and
/// retrying 4000 → 4001 → …, so the silent fallback let *every* channel in a
/// process claim port 4000: all of them then announced the same
/// `tcp://host:4000` member identity over multicast, each channel recognised
/// its peers as itself, and membership never converged (TestTcpFailureDetector
/// saw 0 members, TestNonBlockingCoordinator 4 of 9).
///
/// A literal address is used as-is. A name is resolved and IPv4 is preferred,
/// matching `InetAddress.getByName`'s default ordering (`preferIPv6Addresses`
/// is false by default, and the Tomcat suite additionally runs with
/// `-Djava.net.preferIPv4Stack=true`).
pub(crate) fn single_bind_addr(host: &str, port: u16) -> Result<SocketAddr, std::io::Error> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    // A scoped v6 literal ("fe80::1%3") only parses through the socket-addr
    // resolver, so leave it to the lookup below.
    if !bare.contains('%') {
        if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
            return Ok(SocketAddr::new(ip, port));
        }
    }
    let resolved: Vec<SocketAddr> = (bare, port).to_socket_addrs()?.collect();
    resolved
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| resolved.first())
        .copied()
        .ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::AddrNotAvailable,
                format!("no address resolved for {host}"),
            )
        })
}

fn ssc_bind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("bind: null channel")),
    };
    // arg[2] is the backlog — std::net::TcpListener picks its own, but the
    // AF_UNIX path issues `listen()` itself and does honour it.
    let backlog = int_arg(args, 2).max(0);

    // `ServerSocketChannel.bind(null)` / `bind(null, backlog)` is SPECIFIED:
    // "If the local address is null then the socket will be bound to an
    // address that is assigned automatically." (Same wording on
    // `ServerSocket.bind`, which reaches this native through
    // `ss_wrapper_bind`.) Rejecting it with an IOException broke the ordinary
    // "listen on an ephemeral port on every interface" idiom.
    let Some(sa) = obj_or_none(args, 1) else {
        let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
            .map_err(|e| map_err("0.0.0.0:0", e))?;
        return ssc_finish_bind(ctx, this, listener, 0);
    };

    if let Some(path) = decode_unix_socket_address(ctx, sa)? {
        return ssc_bind_unix(ctx, this, &path, backlog);
    }

    let (host, port) = decode_socket_address(ctx, sa)?;
    // Prefer the numeric address the JDK already resolved into this
    // InetSocketAddress over its hostname text: `getHostString()` hands back
    // "localhost" for `new InetSocketAddress("localhost", p)`, and a *name* is
    // exactly what makes the bind below ambiguous (see `single_bind_addr`).
    let host = sa_resolved_ip(ctx, sa).unwrap_or(host);
    // host==""/"0.0.0.0"/"::" maps to wildcard.
    let bind_addr = if host.is_empty() {
        SocketAddr::from(([0, 0, 0, 0], port))
    } else {
        single_bind_addr(&host, port).map_err(|e| map_err(&format!("{host}:{port}"), e))?
    };
    let listener = TcpListener::bind(bind_addr).map_err(|e| map_err(&bind_addr.to_string(), e))?;
    ssc_finish_bind(ctx, this, listener, port as i32)
}

/// Shared tail of every INET `ServerSocketChannel.bind`: apply the channel's
/// blocking mode to the fresh listener, register it, and record the id + the
/// port the OS actually assigned. `requested_port` is only the fallback for a
/// `local_addr()` that fails.
fn ssc_finish_bind(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    listener: TcpListener,
    requested_port: i32,
) -> MethodCallResult {
    let local_port = listener
        .local_addr()
        .map(|a| a.port() as i32)
        .unwrap_or(requested_port);
    let blocking = read_blocking_flag(ctx, this);
    if !blocking {
        listener
            .set_nonblocking(true)
            .map_err(|e| map_err("set_nonblocking listener", e))?;
    }
    let id = tcp_register(TcpHandle::Listener(listener));
    tcp_blocking_state().write().insert(id, blocking);

    cf_set(ctx, this, F_REG_ID, Value::Int(id));
    cf_set(ctx, this, F_LOCAL_PORT, Value::Int(local_port));
    Ok(Some(Value::Object(Some(this))))
}

/// `ServerSocketChannelImpl.blockingAccept(long nanos)` — the timed accept the
/// JDK's own `ServerSocketAdaptor.accept()` calls when the adapter carries a
/// SO_TIMEOUT, i.e. `ServerSocketChannel.socket().setSoTimeout(ms)` then
/// `accept()`. That is Tomcat's `NioEndpoint.initServerSocket` shape.
///
/// It is declared on `ServerSocketChannelImpl`, but CratonVM's
/// `ServerSocketChannel.open()` hands back an instance of the ABSTRACT
/// `java.nio.channels.ServerSocketChannel`, so the adapter's call landed on a
/// receiver that has no such method: `NoSuchMethodError:
/// java.nio.channels.ServerSocketChannel.blockingAccept(J)` where HotSpot
/// throws `SocketTimeoutException`. Register it on our channel object, exactly
/// as `localAddress()` is registered above and for the same reason.
fn ssc_blocking_accept(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let nanos = match args.get(1) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => i64::from(*n),
        _ => 0,
    };
    // `nanos <= 0` is the JDK's "no timeout" spelling; fall through to the
    // ordinary blocking accept.
    let deadline =
        (nanos > 0).then(|| std::time::Instant::now() + Duration::from_nanos(nanos as u64));
    ssc_accept_impl(ctx, args, deadline)
}

fn ssc_accept(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ssc_accept_impl(ctx, args, None)
}

fn ssc_accept_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    deadline: Option<std::time::Instant>,
) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("accept: null channel")),
    };
    let id = read_reg_id(ctx, this).ok_or_else(|| ioex("accept: server channel not bound"))?;
    let blocking = read_blocking_flag(ctx, this);

    let is_unix_listener = matches!(
        tcp_registry().read().get(&id),
        Some(TcpHandle::UnixListener(_))
    );
    uds_dbg(format_args!(
        "ssc_accept id={id:#x} unix_listener={is_unix_listener} family_unix={}",
        is_unix_family(ctx, this)
    ));
    if is_unix_listener {
        return ssc_accept_unix(ctx, this, id, blocking);
    }

    // Wave 3 Task C: the selector loop pre-drains pending accepts when
    // OP_ACCEPT fires (see `kernel_select_*` in nio_selector.rs); pull
    // from that side-channel first so we don't block on a queue that
    // has already been emptied.
    let preaccepted = crate::nio_selector::take_any_pending_accepted(id);
    let preaccepted_used = preaccepted.is_some();

    if !tcp_listener_is_registered(id) {
        return Err(ioex("accept: id is not a listener"));
    }
    let accepted = if let Some(stream) = preaccepted {
        let peer = stream
            .peer_addr()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
        Some((stream, peer))
    } else {
        // A blocking accept() parks in the OS for an unbounded time (the
        // acceptor thread sits here whenever no connection is pending). It
        // touches no Java heap, so bracket it in a GC-blocking region:
        // otherwise a stop-the-world GC requested while this thread is parked
        // in accept() counts it in `expected` and `wait_for_all` deadlocks
        // forever (the acceptor never reaches an interpreter safepoint). This
        // is especially likely when a single long-lived connection is reused
        // (e.g. HTTP/2), leaving the acceptor idle in accept() for the whole
        // exchange. `end_blocking_region` waits out any active pause before we
        // resume touching the heap below. The actual wait is a close-aware
        // nonblocking poll loop over the REGISTRY's listener (never a private
        // duplicate — see `accept_close_aware`), so `close()` both wakes this
        // path and closes the OS socket in the same instant.
        let res = if let Some(deadline) = deadline {
            // Timed accept (`blockingAccept(nanos)`): the same close-aware poll
            // loop, bounded. Expiry is a `SocketTimeoutException`, NOT a null
            // return — a null would tell `ServerSocketAdaptor.accept()` that a
            // blocking accept produced no socket, which it asserts against.
            ctx.begin_blocking_region();
            let res = accept_until_deadline(id, deadline);
            ctx.end_blocking_region();
            res
        } else if blocking {
            ctx.begin_blocking_region();
            let res = accept_close_aware(id, true);
            ctx.end_blocking_region();
            res
        } else {
            accept_close_aware(id, false)
        };
        match res {
            Ok(Some(pair)) => Some(pair),
            Ok(None) if deadline.is_some() => {
                return Err(RuntimeError::SocketTimeoutException {
                    message: "Accept timed out".into(),
                }
                .into());
            }
            Ok(None) => None,
            Err(e) => return Err(map_err("accept", e)),
        }
    };

    let Some((stream, peer)) = accepted else {
        return Ok(Some(Value::Object(None)));
    };
    // Diagnostic (CRATONVM_DBG_SC_READ=1, shares the read diagnostic's env
    // var — same investigation): log every successful accept() with the
    // NEW child id and whether it came from the selector's pre-drained
    // `pending_accepted` side-channel or a fresh OS `accept()` call. If the
    // StompWebSocketIntegrationTests repro ever shows TWO child ids for
    // what should be one client connection (same peer port), that is a
    // double-accept bug upstream of `sc_read` entirely; if it shows only
    // ONE id (as expected), the duplicate-CONNECT-dispatch investigation
    // stays focused on `sc_read` / the buffer fill-and-parse path.
    if io_flags().dbg_sc_read {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        eprintln!(
            "[SC_ACCEPT] t={ms} listener_id={id:#x} peer={peer} source={}",
            if preaccepted_used {
                "preaccepted"
            } else {
                "fresh"
            }
        );
    }

    // Inherit non-blocking flag of the parent channel.
    let _ = stream.set_nonblocking(!blocking);

    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    let new_id = tcp_register(TcpHandle::Stream(Arc::new(stream)));
    tcp_blocking_state().write().insert(new_id, blocking);

    let child = alloc_obj(ctx, "java/nio/channels/SocketChannel", N_FIELDS);
    let child = init_channel_locks(ctx, child);
    cf_set(ctx, child, F_OPEN, Value::Int(1));
    cf_set(
        ctx,
        child,
        F_BLOCKING,
        Value::Int(if blocking { 1 } else { 0 }),
    );
    cf_set(ctx, child, F_REG_ID, Value::Int(new_id));
    cf_set(ctx, child, F_CONNECTED, Value::Int(1));
    cf_set(ctx, child, F_LOCAL_PORT, Value::Int(local_port));
    let host_str = ctx.create_string(&peer.ip().to_string());
    cf_set(ctx, child, F_REMOTE, Value::Object(Some(host_str)));
    cf_set(ctx, child, F_REMOTE_PORT, Value::Int(peer.port() as i32));

    Ok(Some(Value::Object(Some(child))))
}

/// Close-aware `accept()` on an AF_UNIX listener.
///
/// The socket is flipped non-blocking and polled, exactly like
/// `accept_close_aware` does for TCP: a blocking `accept()` would have to be
/// issued while holding the registry lock, which would deadlock against the
/// `close()` that is supposed to wake it. Polling keeps each lock acquisition
/// to a single non-blocking syscall and lets `close()` — which removes the
/// registry entry — end the wait promptly.
///
/// This path was always registry-borrowed because an `UdsListener` cannot be
/// `try_clone`d out of the map. The TCP twin *could* be, and was, which is
/// how it ended up keeping the listening socket open across a `close()` — see
/// `accept_close_aware`. The two now have the same shape.
fn uds_accept_close_aware(
    id: i32,
    blocking: bool,
) -> std::io::Result<Option<(TcpStream, String)>> {
    loop {
        let attempt = {
            let map = tcp_registry().read();
            match map.get(&id) {
                Some(TcpHandle::UnixListener(l)) => {
                    l.set_nonblocking(true)?;
                    l.accept()
                }
                _ => {
                    return Err(std::io::Error::new(
                        ErrorKind::Interrupted,
                        "server channel closed",
                    ))
                }
            }
        };
        match attempt {
            Ok(pair) => return Ok(Some(pair)),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if !blocking {
                    return Ok(None);
                }
                std::thread::sleep(ACCEPT_CLOSE_POLL);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// `ServerSocketChannel.accept()` for a Unix-domain listener. Mirrors the TCP
/// branch of `ssc_accept`, minus the selector pre-drain (which only ever
/// stashes `TcpStream`s from INET listeners) and the peer host/port, which do
/// not exist for AF_UNIX — the child channel carries the server's path as its
/// address instead, matching what the JDK reports.
fn ssc_accept_unix(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    id: i32,
    blocking: bool,
) -> MethodCallResult {
    // Bracket the wait in a GC-blocking region for the same reason the TCP
    // path does: an idle acceptor parks here for an unbounded time and
    // reaches no interpreter safepoint, so a concurrent stop-the-world pause
    // would otherwise wait on it forever.
    uds_dbg(format_args!("accept enter id={id:#x} blocking={blocking}"));
    let res = if blocking {
        ctx.begin_blocking_region();
        let res = uds_accept_close_aware(id, true);
        ctx.end_blocking_region();
        res
    } else {
        uds_accept_close_aware(id, false)
    };
    let accepted = match res {
        Ok(pair) => pair,
        Err(e) => {
            uds_dbg(format_args!("accept id={id:#x} error={e}"));
            return Err(map_err("accept", e));
        }
    };
    uds_dbg(format_args!(
        "accept id={id:#x} -> {}",
        match &accepted {
            Some((_, peer)) => format!("connection peer=\"{peer}\""),
            None => "none (would block)".to_string(),
        }
    ));
    let Some((stream, _peer)) = accepted else {
        return Ok(Some(Value::Object(None)));
    };
    // Inherit the parent channel's blocking mode, like the TCP path.
    let _ = stream.set_nonblocking(!blocking);

    let new_id = tcp_register(TcpHandle::Stream(Arc::new(stream)));
    tcp_blocking_state().write().insert(new_id, blocking);

    // The accepted socket's own name is unnamed on both Windows and Linux;
    // report the listener's path, which is the address the JDK surfaces from
    // `getLocalAddress()`/`getRemoteAddress()` on an accepted UDS channel.
    let path = cf_get_str(ctx, this, F_UDS_PATH).unwrap_or_default();

    let child = alloc_obj(ctx, "java/nio/channels/SocketChannel", N_FIELDS);
    let child = init_channel_locks(ctx, child);
    cf_set(ctx, child, F_OPEN, Value::Int(1));
    cf_set(
        ctx,
        child,
        F_BLOCKING,
        Value::Int(if blocking { 1 } else { 0 }),
    );
    cf_set(ctx, child, F_REG_ID, Value::Int(new_id));
    cf_set(ctx, child, F_CONNECTED, Value::Int(1));
    cf_set(ctx, child, F_LOCAL_PORT, Value::Int(0));
    cf_set(ctx, child, F_FAMILY, Value::Int(FAMILY_UNIX));
    let path_str = ctx.create_string(&path);
    cf_set(ctx, child, F_UDS_PATH, Value::Object(Some(path_str)));

    Ok(Some(Value::Object(Some(child))))
}

fn ssc_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    sc_close(ctx, args)
}

// ---------------------------------------------------------------------------
// Public registration
// ---------------------------------------------------------------------------

/// Register all SocketChannel / ServerSocketChannel natives that back the
/// real-JDK `sun.nio.ch.*Impl` classes plus the `java.nio.channels.*`
/// factories. Idempotent: callers may register multiple times — later
/// registrations win at the same triple.
pub fn register_socket_channel_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sc = "java/nio/channels/SocketChannel";
    let scimpl = "sun/nio/ch/SocketChannelImpl";
    let ssc = "java/nio/channels/ServerSocketChannel";
    let sscimpl = "sun/nio/ch/ServerSocketChannelImpl";

    // -- SelectorProvider factory methods (JDK 21+ / Netty) --
    // Netty's `NioServerSocketChannel`/`NioSocketChannel` build their JDK channel
    // by calling `provider.openServerSocketChannel()` / `provider.openSocketChannel()`
    // DIRECTLY on the cached `SelectorProvider` instance (on Windows JDK 21+ that is
    // `sun.nio.ch.WEPollSelectorProvider`), NOT the static
    // `ServerSocketChannel.open()` / `SocketChannel.open()` handled below. The real
    // provider builds a JDK `ServerSocketChannelImpl`/`SocketChannelImpl` that is not a
    // valid CratonVM channel — Netty's `AbstractChannel.register0` then sees
    // `isOpen() == false` and throws `ClosedChannelException`, killing the Vert.x/Netty
    // HTTP server bind (Keycloak/Quarkus). Route these provider factories to our own
    // channel factories (which `ServerSocketChannel.open()` already uses). Register on
    // BOTH the concrete provider and its declaring base so we match regardless of
    // whether native dispatch keys on the receiver's concrete class or the resolved
    // method's class. `ssc_open`/`sc_open` ignore the receiver arg, so the instance
    // form is safe.
    for prov in [
        "sun/nio/ch/WEPollSelectorProvider",
        "sun/nio/ch/EPollSelectorProvider",
        "sun/nio/ch/SelectorProviderImpl",
    ] {
        r.register(
            prov,
            "openServerSocketChannel",
            "()Ljava/nio/channels/ServerSocketChannel;",
            ssc_open,
        );
        r.register(
            prov,
            "openSocketChannel",
            "()Ljava/nio/channels/SocketChannel;",
            sc_open,
        );
        // JDK 16+ family-aware provider factories — the route
        // `ServerSocketChannel.open(StandardProtocolFamily.UNIX)` takes.
        r.register(
            prov,
            "openServerSocketChannel",
            "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/ServerSocketChannel;",
            ssc_open_family,
        );
        r.register(
            prov,
            "openSocketChannel",
            "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/SocketChannel;",
            sc_open_family,
        );
    }

    // -- SocketChannel factory + lifecycle --
    for c in [sc, scimpl] {
        r.register(c, "open", "()Ljava/nio/channels/SocketChannel;", sc_open);
        r.register(
            c,
            "open",
            "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;",
            sc_open_connected,
        );
        // JDK 16+: `open(ProtocolFamily)` — Unix-domain sockets. Without this
        // the call falls through to real JDK bytecode, which builds a genuine
        // `sun.nio.ch.SocketChannelImpl` that none of CratonVM's channel
        // natives (read/write/selector registration) can drive.
        r.register(
            c,
            "open",
            "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/SocketChannel;",
            sc_open_family,
        );
        r.register(c, "isOpen", "()Z", sc_is_open);
        r.register(c, "isBlocking", "()Z", sc_is_blocking);
        r.register(c, "isConnected", "()Z", sc_is_connected);
        r.register(c, "socket", "()Ljava/net/Socket;", sc_socket);
        r.register(
            c,
            "getRemoteAddress",
            "()Ljava/net/SocketAddress;",
            sc_remote_address,
        );
        r.register(
            c,
            "getLocalAddress",
            "()Ljava/net/SocketAddress;",
            sc_local_address,
        );
        // Package-private `localAddress()`/`remoteAddress()` — these are what
        // sun.nio.ch.SocketAdaptor (the object returned by socket()) delegates
        // to for getLocalSocketAddress()/getRemoteSocketAddress(). Without them,
        // SocketAdaptor.getLocalSocketAddress NoSuchMethodErrors — which is the
        // path Gradle's TcpOutgoingConnector.detectSelfConnect takes.
        r.register(
            c,
            "localAddress",
            "()Ljava/net/SocketAddress;",
            sc_local_address,
        );
        r.register(
            c,
            "remoteAddress",
            "()Ljava/net/SocketAddress;",
            sc_remote_address,
        );
        r.register(
            c,
            "configureBlocking",
            "(Z)Ljava/nio/channels/SelectableChannel;",
            sc_configure_blocking,
        );
        r.register(
            c,
            "configureBlocking",
            "(Z)Ljava/nio/channels/spi/AbstractSelectableChannel;",
            sc_configure_blocking,
        );
        r.register(c, "close", "()V", sc_close);
        // The reactor (and JDK code) often closes via the FINAL
        // `AbstractInterruptibleChannel.close()` rather than the overridable
        // `SocketChannel.close()`. That bytecode runs (closeLock seeded by
        // init_channel_locks) and calls the abstract `implCloseSelectableChannel()`
        // — which our synthetic SocketChannel class does not implement. Register
        // it as the actual socket teardown so the close completes instead of
        // hitting an AbstractMethodError. (`AbstractSelectableChannel.implCloseChannel`
        // then cancels keys under keyLock with keyCount==0 — a no-op for us.)
        r.register(c, "implCloseSelectableChannel", "()V", sc_close);
        r.register(c, "implCloseChannel", "()V", sc_close);
        r.register(c, "connect", "(Ljava/net/SocketAddress;)Z", sc_connect);
        // `SocketChannel.bind` covariantly returns SocketChannel. Hazelcast
        // reaches it through SocketAdaptor.bind before registering its client
        // connection; without this exact descriptor dispatch falls through to
        // the abstract no-Code declaration and throws AbstractMethodError.
        r.register(
            c,
            "bind",
            "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;",
            sc_bind,
        );
        // Keep the superinterface descriptor reachable too for callers whose
        // invokeinterface resolution preserves NetworkChannel's declaration.
        r.register(
            c,
            "bind",
            "(Ljava/net/SocketAddress;)Ljava/nio/channels/NetworkChannel;",
            sc_bind,
        );
        r.register(
            c,
            "blockingConnect",
            "(Ljava/net/SocketAddress;J)V",
            sc_blocking_connect,
        );
        r.register(c, "finishConnect", "()Z", sc_finish_connect);
        r.register(c, "isConnectionPending", "()Z", sc_is_connection_pending);
        r.register(c, "isInputOpen", "()Z", |ctx, args| {
            sc_io_open(ctx, args, F_INPUT_SHUTDOWN)
        });
        r.register(c, "isOutputOpen", "()Z", |ctx, args| {
            sc_io_open(ctx, args, F_OUTPUT_SHUTDOWN)
        });
        r.register(c, "shutdownInput", "()Ljava/nio/channels/SocketChannel;", |ctx, args| {
            sc_shutdown(ctx, args, std::net::Shutdown::Read, F_INPUT_SHUTDOWN, "shutdownInput")
        });
        r.register(c, "shutdownOutput", "()Ljava/nio/channels/SocketChannel;", |ctx, args| {
            sc_shutdown(ctx, args, std::net::Shutdown::Write, F_OUTPUT_SHUTDOWN, "shutdownOutput")
        });
        r.register(c, "read", "(Ljava/nio/ByteBuffer;)I", sc_read);
        r.register(c, "write", "(Ljava/nio/ByteBuffer;)I", sc_write);
        // The byte[] pair the `Socket` VIEW of a channel reads and writes
        // through: `SocketAdaptor.getInputStream()` hands back a
        // `sun.nio.ch.SocketInputStream` whose `implRead` calls
        // `blockingRead`, and the output stream's `implWrite` calls
        // `blockingWriteFully`. Both are declared on `SocketChannelImpl`,
        // which our channel object is not, so without these the adapter's
        // streams died with `NoSuchMethodError:
        // java.nio.channels.SocketChannel.blockingRead([BIIJ)I` — an accepted
        // Socket that could not be read from. Same shape as `blockingAccept`.
        r.register(c, "blockingRead", "([BIIJ)I", sc_blocking_read);
        r.register(c, "blockingWriteFully", "([BII)V", sc_blocking_write_fully);
        // Vectored (scattering read / gathering write). Abstract on
        // SocketChannel (inherited from Scattering/GatheringByteChannel); the
        // synthetic channel object has no concrete body, so register both the
        // 3-arg slice form and the 1-arg convenience form. Tomcat's websocket
        // write path flushes header+body via the gathering form (DF03).
        r.register(c, "read", "([Ljava/nio/ByteBuffer;II)J", sc_read_scattering);
        r.register(c, "read", "([Ljava/nio/ByteBuffer;)J", sc_read_scattering);
        r.register(
            c,
            "write",
            "([Ljava/nio/ByteBuffer;II)J",
            sc_write_gathering,
        );
        r.register(c, "write", "([Ljava/nio/ByteBuffer;)J", sc_write_gathering);
        r.register(
            c,
            "setOption",
            "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
            sc_set_option,
        );
        // `SocketChannel.setOption` covariantly narrows the return type to
        // `SocketChannel` (vs `NetworkChannel.setOption`), so the abstract
        // declaration + every concrete call site uses the
        // `...)Ljava/nio/channels/SocketChannel;` descriptor. Without this
        // overload the native is missed and dispatch hits the abstract
        // `SocketChannel.setOption` (no Code attribute) → AbstractMethodError.
        // Tomcat's `NioEndpoint.setSocketOptions` calls this on EVERY accepted
        // connection ("Error setting socket options"), which aborts the socket
        // before the request is read → the connector resets every request
        // without responding (embedded-server serving wall; bug 10 / group 04).
        r.register(
            c,
            "setOption",
            "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/SocketChannel;",
            sc_set_option,
        );
        r.register(
            c,
            "getOption",
            "(Ljava/net/SocketOption;)Ljava/lang/Object;",
            sc_get_option,
        );
        r.register(
            c,
            "supportedOptions",
            "()Ljava/util/Set;",
            sc_supported_options,
        );
    }

    // -- ServerSocketChannel factory + lifecycle --
    for c in [ssc, sscimpl] {
        r.register(
            c,
            "open",
            "()Ljava/nio/channels/ServerSocketChannel;",
            ssc_open,
        );
        // JDK 16+: `open(ProtocolFamily)` — Unix-domain sockets. This is what
        // `NioEndpoint.initServerSocket()` calls for a connector configured
        // with `unixDomainSocketPath`; without it the call falls through to
        // real JDK bytecode, whose `ServerSocketChannelImpl` is not a channel
        // any of CratonVM's channel natives can drive.
        r.register(
            c,
            "open",
            "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/ServerSocketChannel;",
            ssc_open_family,
        );
        r.register(c, "socket", "()Ljava/net/ServerSocket;", ssc_socket);
        r.register(c, "isOpen", "()Z", sc_is_open);
        r.register(c, "isBlocking", "()Z", sc_is_blocking);
        // `isBound()Z` is not declared on the abstract `ServerSocketChannel`, but
        // `sun.nio.ch.ServerSocketAdaptor.isBound()` (returned by `socket()`) and
        // Netty's `NioServerSocketChannel.isActive()` call it on our channel
        // object (whose runtime class is `java/nio/channels/ServerSocketChannel`).
        // Our native dispatch resolves by (class, name, desc), so registering it
        // here makes the call succeed instead of NoSuchMethodError.
        r.register(c, "isBound", "()Z", ssc_is_bound);
        r.register(
            c,
            "configureBlocking",
            "(Z)Ljava/nio/channels/SelectableChannel;",
            sc_configure_blocking,
        );
        r.register(
            c,
            "configureBlocking",
            "(Z)Ljava/nio/channels/spi/AbstractSelectableChannel;",
            sc_configure_blocking,
        );
        r.register(c, "close", "()V", ssc_close);
        // See the SocketChannel loop: handle the real-close abstract hooks so a
        // close via the final AbstractInterruptibleChannel.close() completes.
        r.register(c, "implCloseSelectableChannel", "()V", ssc_close);
        r.register(c, "implCloseChannel", "()V", ssc_close);
        r.register(
            c,
            "bind",
            "(Ljava/net/SocketAddress;I)Ljava/nio/channels/ServerSocketChannel;",
            ssc_bind,
        );
        r.register(
            c,
            "bind",
            "(Ljava/net/SocketAddress;)Ljava/nio/channels/NetworkChannel;",
            ssc_bind,
        );
        // COVARIANT RETURN (the SocketChannel loop above documents the same
        // hazard for `bind`/`setOption`): `ServerSocketChannel.bind` narrows
        // `NetworkChannel.bind`'s return type, so `ssc.bind(addr)` compiles to
        // `...)Ljava/nio/channels/ServerSocketChannel;` — the descriptor a
        // textbook `ServerSocketChannel.open().bind(new InetSocketAddress(0))`
        // uses. That triple was NOT registered here, so in synthetic-JDK mode
        // the stale 1-arg synthetic in
        // `native-builtins/phases_late/net_channels.rs` (registered earlier,
        // and therefore not overwritten) won: it recorded its listener in the
        // fd table + object field 2, while `accept()` below reads the
        // identity-keyed `chan_fields` table — so every 1-arg bind was
        // followed by `accept: server channel not bound`. Register the
        // covariant descriptor so the whole ServerSocketChannel surface comes
        // from one implementation.
        r.register(
            c,
            "bind",
            "(Ljava/net/SocketAddress;)Ljava/nio/channels/ServerSocketChannel;",
            ssc_bind,
        );
        r.register(
            c,
            "accept",
            "()Ljava/nio/channels/SocketChannel;",
            ssc_accept,
        );
        // The timed sibling `ServerSocketAdaptor.accept()` calls when the
        // adapter has a SO_TIMEOUT — see `ssc_blocking_accept`.
        r.register(
            c,
            "blockingAccept",
            "(J)Ljava/nio/channels/SocketChannel;",
            ssc_blocking_accept,
        );
        r.register(
            c,
            "setOption",
            "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
            sc_set_option,
        );
        // Covariant return: `ServerSocketChannel.setOption` returns
        // `ServerSocketChannel` (NioEndpoint sets options on the listening
        // channel at bind time); register that descriptor too so the call does
        // not fall through to the abstract declaration. See the SocketChannel
        // note above.
        r.register(
            c,
            "setOption",
            "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/ServerSocketChannel;",
            sc_set_option,
        );
        r.register(
            c,
            "getOption",
            "(Ljava/net/SocketOption;)Ljava/lang/Object;",
            sc_get_option,
        );
        r.register(
            c,
            "supportedOptions",
            "()Ljava/util/Set;",
            sc_supported_options,
        );
        r.register(
            c,
            "getLocalAddress",
            "()Ljava/net/SocketAddress;",
            ssc_local_address,
        );
        // Package-private `localAddress()` is what `sun.nio.ch.ServerSocketAdaptor`
        // (returned by `socket()`, via getInetAddress/getLocalPort) and Netty read the
        // bound address through. It lives on `ServerSocketChannelImpl`, not the abstract
        // `ServerSocketChannel`, so register it on our channel object too (mirrors the
        // SocketChannel side above). Without it the bind Runnable throws NoSuchMethodError
        // (swallowed by the event loop), so listen() never fully completes.
        r.register(
            c,
            "localAddress",
            "()Ljava/net/SocketAddress;",
            ssc_local_address,
        );
    }

    // -- ServerSocket adapter (used by ServerSocketChannel.socket()) --
    // The wrapper returned by `ssc.socket()` has a back-ref to its parent
    // SSC at SS_CHANNEL_REF. The bind / getLocalPort / close /
    // getInetAddress methods detect this back-ref and delegate to the
    // owning channel; without a back-ref they delegate OUT to the plain
    // owner in native-builtins, so plain `new ServerSocket()` use cases are
    // not perturbed.
    //
    // `accept` is deliberately NOT one of the wrappers — it stays with
    // net_phase_e's RE.2 native, which owns every plain ServerSocket's
    // listener. Wrapping it here would make this crate the winner for every
    // accept in the VM and put a cross-crate hop in front of the common case.
    // Instead we hand RE.2 a way to bounce the adapter case back to us.
    cratonvm_native_api::plain_server_socket::set_channel_backed_accept(
        ss_adapter_channel_accept,
    );
    let server_socket = "java/net/ServerSocket";
    r.register(
        server_socket,
        "bind",
        "(Ljava/net/SocketAddress;)V",
        ss_wrapper_bind,
    );
    r.register(
        server_socket,
        "bind",
        "(Ljava/net/SocketAddress;I)V",
        ss_wrapper_bind_backlog,
    );
    r.register(server_socket, "getLocalPort", "()I", ss_wrapper_local_port);
    r.register(
        server_socket,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        ss_wrapper_local_address,
    );
    r.register(server_socket, "isBound", "()Z", ss_wrapper_is_bound);
    r.register(server_socket, "isClosed", "()Z", ss_wrapper_is_closed);
    r.register(server_socket, "close", "()V", ss_wrapper_close);

    // Option setters on the `java.net.Socket` adapter returned by
    // SocketChannel.socket(). These are the methods Tomcat's
    // SocketProperties.setProperties invokes; the adapter has no real
    // SocketImpl so the real bytecode would NPE in getImpl(). The adaptor
    // owns its own setSoTimeout implementation, so do not shadow the base
    // Socket setter: ordinary sockets must apply SO_RCVTIMEO to their stream.
    let client_socket = "java/net/Socket";
    for (m, d, cb) in [
        (
            "setReceiveBufferSize",
            "(I)V",
            socket_opt_set_rcvbuf as cratonvm_native_api::registry::NativeCallback,
        ),
        ("setSendBufferSize", "(I)V", socket_opt_set_sndbuf),
        ("setKeepAlive", "(Z)V", socket_opt_set_keepalive),
        ("setReuseAddress", "(Z)V", socket_opt_set_reuseaddr),
        ("setTcpNoDelay", "(Z)V", socket_opt_set_nodelay),
        ("setOOBInline", "(Z)V", socket_opt_set_oobinline),
        ("setSoLinger", "(ZI)V", socket_opt_set_linger),
        ("setPerformancePreferences", "(III)V", socket_opt_noop),
    ] {
        // Do NOT shadow a real implementation an earlier registrar already
        // installed for this key. This block runs late (`register_io_natives`
        // is called after `register_essential_natives_with_shims`), so without
        // this guard it silently replaced, for every `java.net.Socket` in the
        // VM, whatever real setter phases_early/net_phase_e had provided —
        // last-writer-wins. The channel-adaptor case that these handlers exist
        // for only ever needs them when nobody else claimed the key.
        if r.find(client_socket, m, d).is_some() {
            continue;
        }
        r.register(client_socket, m, d, cb);
    }
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// ServerSocketChannel.socket() — wrapper ServerSocket
// ---------------------------------------------------------------------------

// The channel-backed wrapper is a plain `java/net/ServerSocket` allocated
// with the JDK's real layout; we cannot stash a back-ref inside one of its
// fields without colliding with JDK-private slots. Instead we keep the
// wrapper → channel mapping in a process-wide side-table.
//
// C27 (Round-11 GC-safety fix): the table was previously keyed by
// `ss.as_ptr() as usize` and stored `ssc.as_ptr() as usize` as the
// value. Under a moving collector that pairing is doubly unsafe:
//
//   1. The key becomes stale when the GC compacts the wrapper, so a
//      lookup with the relocated wrapper's pointer misses; worse, a
//      fresh object allocated at the wrapper's old address silently
//      collides with the stale row.
//   2. The value was resurrected via `unsafe { ObjectRef::from_raw(raw
//      as *mut u8) }` even though no Java root kept the SSC alive — the
//      side-table itself was not scanned, so the SSC could be reclaimed
//      while a wrapper still tried to dispatch through it (use-after-
//      free), or relocated so the stored pointer now refers to garbage.
//
// The fix re-keys on the wrapper's GC-stable identity hash code and
// stores the SSC as an `ObjectRef` directly. A post-compaction hook
// (`ss_back_ref_update_after_gc`) remaps the stored values when the GC
// fires; the keys are GC-stable on their own (the GC carries the hash
// word across moves — see `gc/src/compact_header.rs::HashCodeTable::
// update_after_gc`). Mirrors `SEED_TABLE` in
// `native-builtins/src/securerandom.rs` and the C21 collection-overlay
// fix in `native-collections/src/lib.rs`.
const SSC_SOCKET_CACHE: usize = 5; // unused F_REMOTE slot — see note below.

/// One row per live ServerSocket wrapper. The hash only chooses a bucket:
/// Java identity hashes are not unique, so every lookup also matches the
/// wrapper receiver itself.
struct SsBackRef {
    wrapper: ObjectRef,
    channel: ObjectRef,
}

fn ss_back_ref_table() -> &'static RwLock<rustc_hash::FxHashMap<i32, Vec<SsBackRef>>> {
    static REG: OnceLock<RwLock<rustc_hash::FxHashMap<i32, Vec<SsBackRef>>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(rustc_hash::FxHashMap::default()))
}

fn ss_record_back_ref(ctx: &mut dyn NativeContext, ss: ObjectRef, ssc: ObjectRef) {
    let key = ctx.identity_hash_code(ss);
    let mut table = ss_back_ref_table().write();
    let bucket = table.entry(key).or_default();
    if let Some(row) = bucket.iter_mut().find(|row| row.wrapper == ss) {
        row.channel = ssc;
    } else {
        bucket.push(SsBackRef {
            wrapper: ss,
            channel: ssc,
        });
    }
}

fn ss_back_ref(ctx: &mut dyn NativeContext, ss: ObjectRef) -> Option<ObjectRef> {
    let key = ctx.identity_hash_code(ss);
    ss_back_ref_table()
        .read()
        .get(&key)
        .and_then(|bucket| bucket.iter().find(|row| row.wrapper == ss))
        .map(|row| row.channel)
}

fn ss_remove_back_ref(ctx: &mut dyn NativeContext, ss: ObjectRef) {
    let key = ctx.identity_hash_code(ss);
    let mut table = ss_back_ref_table().write();
    let remove_bucket = if let Some(bucket) = table.get_mut(&key) {
        bucket.retain(|row| row.wrapper != ss);
        bucket.is_empty()
    } else {
        false
    };
    if remove_bucket {
        table.remove(&key);
    }
}

/// Keep both ends of the adapter mapping alive during a collection. The
/// mapping is dropped promptly by `ServerSocket.close`, so this is not a
/// lifetime extension for closed endpoints.
pub fn gc_scan_ss_back_ref_roots(roots: &mut Vec<ObjectRef>) {
    let table = ss_back_ref_table().read();
    for bucket in table.values() {
        for row in bucket {
            roots.push(row.wrapper);
            roots.push(row.channel);
        }
    }
}

/// Relocate both receiver and channel references after moving GC. The
/// identity-hash bucket remains stable while its ObjectRef discriminator must
/// be updated to preserve collision-safe lookup.
pub fn ss_back_ref_update_after_gc<S: std::hash::BuildHasher>(
    pointer_map: &std::collections::HashMap<usize, usize, S>,
) {
    if pointer_map.is_empty() {
        return;
    }
    let remap = |obj: ObjectRef| {
        let old = obj.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            // SAFETY: this runs during stop-the-world root remapping and the
            // map entry is the non-null forwarding address for `obj`.
            unsafe { ObjectRef::from_raw(new_addr as *mut u8) }
        } else {
            obj
        }
    };
    let mut table = ss_back_ref_table().write();
    for bucket in table.values_mut() {
        for row in bucket {
            row.wrapper = remap(row.wrapper);
            row.channel = remap(row.channel);
        }
    }
}

// `F_REMOTE` (slot 5) of a SSC object is unused for ServerSocketChannel
// instances (only SocketChannel uses it). We hijack it to cache the
// `socket()` adapter so the same instance is returned each call —
// matching java.nio.channels.ServerSocketChannel.socket()'s contract.

fn ssc_socket(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("socket: null channel")),
    };
    // As in `sc_socket`: a Unix-domain listener has no `java.net.ServerSocket`
    // view, and the real `ServerSocketChannelImpl.socket()` throws here too.
    if is_unix_family(ctx, this) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: "Not supported".into(),
        }
        .into());
    }
    if ctx.object_num_fields(this) > SSC_SOCKET_CACHE {
        if let Value::Object(Some(cached)) = ctx.get_field(this, SSC_SOCKET_CACHE) {
            return Ok(Some(Value::Object(Some(cached))));
        }
    }
    // Under CRATONVM_REAL_NET_SOCKETS the central registry filter drops every
    // java/net/ServerSocket native, so real ServerSocket bytecode runs. A bare
    // `new java/net/ServerSocket` allocated WITHOUT its <init> leaves
    // `socketLock` (a `final Object` instance-initializer field) null, so the
    // first real method that does `synchronized (socketLock)` — e.g. getImpl()
    // reached from ServerSocket.setSoTimeout() — throws "monitorenter ... null".
    // This is exactly Tomcat's NioEndpoint.initServerSocket → setProperties →
    // setSoTimeout path. Mirror the real ServerSocketChannelImpl.socket()
    // (return ServerSocketAdaptor.create(this)): the adaptor is a proper
    // java.net.ServerSocket subclass whose bind/accept/setSoTimeout delegate to
    // the channel and whose construction runs the ServerSocket instance
    // initializers (socketLock = new Object()), so getImpl() is never reached.
    if io_flags().real_net_sockets {
        if let Ok(Some(v @ Value::Object(Some(adaptor)))) = ctx.invoke(
            "sun/nio/ch/ServerSocketAdaptor",
            "create",
            "(Lsun/nio/ch/ServerSocketChannelImpl;)Ljava/net/ServerSocket;",
            &[Value::Object(Some(this))],
        ) {
            if ctx.object_num_fields(this) > SSC_SOCKET_CACHE {
                ctx.set_field(this, SSC_SOCKET_CACHE, Value::Object(Some(adaptor)));
            }
            return Ok(Some(v));
        }
        // Fall through to the bare-ServerSocket fallback on any failure.
    }
    // Allocate a real-layout ServerSocket and remember the channel back-ref
    // in a side-table; we cannot stash anything inside the wrapper itself
    // without clashing with JDK-private fields like `bound` or `impl`.
    let ss_value = ctx
        .new_object("java/net/ServerSocket")
        .ok()
        .and_then(|v| match v {
            Some(Value::Object(Some(o))) => Some(o),
            _ => None,
        })
        .ok_or_else(|| ioex("socket: could not allocate ServerSocket"))?;
    ss_record_back_ref(ctx, ss_value, this);
    if ctx.object_num_fields(this) > SSC_SOCKET_CACHE {
        ctx.set_field(this, SSC_SOCKET_CACHE, Value::Object(Some(ss_value)));
    }
    Ok(Some(Value::Object(Some(ss_value))))
}

/// The address Netty publishes from a listener must be usable as a client
/// destination. Windows rejects a connect to an unspecified (`0.0.0.0`/`::`)
/// listener address with WSAEADDRNOTAVAIL, even though binding that wildcard is
/// valid. A local in-process client should therefore receive the corresponding
/// loopback address while concrete listener addresses remain unchanged.
fn advertised_listener_host(addr: SocketAddr) -> String {
    match addr {
        SocketAddr::V4(addr) if addr.ip().is_unspecified() => "127.0.0.1".to_string(),
        SocketAddr::V6(addr) if addr.ip().is_unspecified() => "::1".to_string(),
        _ => addr.ip().to_string(),
    }
}

fn ssc_local_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    if is_unix_family(ctx, this) {
        // Tomcat's `NioEndpoint.getLocalAddress()` is `instanceof
        // InetSocketAddress`-guarded, so returning the UDS address here
        // correctly leaves `Connector.getLocalPort()` at -1, as on HotSpot.
        let Some(path) = cf_get_str(ctx, this, F_UDS_PATH) else {
            return Ok(Some(Value::Object(None)));
        };
        return new_unix_socket_address(ctx, &path);
    }
    let port = cf_get(ctx, this, F_LOCAL_PORT).as_int().unwrap_or(0);
    let id = cf_get(ctx, this, F_REG_ID).as_int().unwrap_or(-1);
    if id < 0 || port <= 0 {
        return Ok(Some(Value::Object(None)));
    }
    // Build via the REAL `InetSocketAddress(String,int)` constructor (as
    // `sc_local_address`/`sc_remote_address` already do) rather than a flat
    // 2-slot synthetic. The real `getPort()`/`getHostString()` bytecode reads
    // `this.holder.port` / `this.holder.hostname`; a flat object has a null
    // `holder`, so `getPort()` returns garbage. Tomcat's
    // `NioEndpoint.getLocalPort()` does
    // `((InetSocketAddress) serverSock.getLocalAddress()).getPort()`, so the
    // flat object made it return -1 — which is the port `TomcatBaseTest.getPort()`
    // hands to `SimpleHttpClient`, so every embedded-server test connected to
    // ":-1" and hung. The real ctor populates the holder and fixes getLocalPort.
    //
    // Host: originally hardcoded to the literal string `"0.0.0.0"` since only
    // `getLocalPort()` mattered for the Tomcat fix above. That broke any
    // caller that DOES use the host — `com.sun.net.httpserver.HttpsServer`
    // (real `sun.net.httpserver.ServerImpl`, which binds a
    // `ServerSocketChannel` and later calls `getLocalAddress()` to answer its
    // own `getAddress()`) handed that bogus `0.0.0.0` back to
    // `RestClientBuilderIntegTests`, which reconnects using it — `0.0.0.0` is
    // not a valid TLS connect target (`WSAEADDRNOTAVAIL`). Look up the
    // listener's REAL bound address instead; falls back to the old
    // `"0.0.0.0"` wildcard text only if the registry entry is gone (channel
    // already closed) or isn't actually a listener.
    let host = match tcp_registry().read().get(&id) {
        Some(TcpHandle::Listener(l)) => l
            .local_addr()
            .map(advertised_listener_host)
            .unwrap_or_else(|_| "0.0.0.0".to_string()),
        _ => "0.0.0.0".to_string(),
    };
    new_resolved_inet_socket_address(ctx, &host, port)
}

fn new_resolved_inet_socket_address(
    ctx: &mut dyn NativeContext,
    host: &str,
    port: i32,
) -> MethodCallResult {
    // Cross-call GC-safety: a freshly created String handed straight into a
    // re-entrant `invoke` has no root of its own between `create_string` and
    // the callee reading it off the operand stack, and both calls below run
    // Java (and therefore can collect). `net_accept` already pins for exactly
    // this window — it surfaced there as `obj_arg` failing on args[1] inside
    // the `InetSocketAddress(String,int)` native with "null object argument".
    // The `InetAddress` `getByName` returns needs the same treatment before it
    // becomes the second call's argument.
    let mut scope = NativeHandleScope::new(ctx);
    let h = scope.create_string(host);
    let h_h = scope.root(h);
    let h_cur = scope.get(&h_h);
    let by_name = scope.invoke(
        "java/net/InetAddress",
        "getByName",
        "(Ljava/lang/String;)Ljava/net/InetAddress;",
        &[Value::Object(Some(h_cur))],
    );
    if let Ok(Some(Value::Object(Some(addr)))) = by_name {
        let addr_h = scope.root(addr);
        let addr_cur = scope.get(&addr_h);
        return scope.new_object_initialized(
            "java/net/InetSocketAddress",
            "(Ljava/net/InetAddress;I)V",
            &[Value::Object(Some(addr_cur)), Value::Int(port)],
        );
    }
    let h_cur = scope.get(&h_h);
    scope.new_object_initialized(
        "java/net/InetSocketAddress",
        "(Ljava/lang/String;I)V",
        &[Value::Object(Some(h_cur)), Value::Int(port)],
    )
}

fn ss_wrapper_bind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("bind: null this")),
    };
    let Some(ssc) = ss_back_ref(ctx, this) else {
        // Plain ServerSocket (no ServerSocketChannel back-ref). This native is
        // the last-registered — and therefore winning — `bind`, but the real
        // binding logic (TcpListener + the `s2` listener table that `accept()`
        // reads + the per-socket state) lives in native-builtins, which we
        // cannot call directly. Delegate through the handler set it installs
        // (BUG-04); previously this no-opped, leaving `getLocalPort()` = 0.
        return plain_server_socket_delegate(ctx, args, |ops| ops.bind, |_| Ok(None));
    };
    // Delegate to ssc_bind with backlog=0 (TcpListener picks its own).
    let sa = args.get(1).copied().unwrap_or(Value::Object(None));
    let backlog = Value::Int(0);
    let _ = ssc_bind(ctx, &[Value::Object(Some(ssc)), sa, backlog])?;
    Ok(None)
}

fn ss_wrapper_bind_backlog(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("bind: null this")),
    };
    let Some(ssc) = ss_back_ref(ctx, this) else {
        // Plain ServerSocket — delegate to the native-builtins plain-bind
        // handler (BUG-04); see ss_wrapper_bind above.
        return plain_server_socket_delegate(ctx, args, |ops| ops.bind, |_| Ok(None));
    };
    let sa = args.get(1).copied().unwrap_or(Value::Object(None));
    let backlog = args.get(2).copied().unwrap_or(Value::Int(50));
    let _ = ssc_bind(ctx, &[Value::Object(Some(ssc)), sa, backlog])?;
    Ok(None)
}

/// Route one plain-`ServerSocket` call to the handler `cratonvm-native-builtins`
/// installed, or answer `fallback` when there is none.
///
/// A missing handler set means the synthetic `java/net/ServerSocket` surface was
/// never registered — i.e. `CRATONVM_REAL=net-sockets` (the default), where the
/// registry drops every native on that class and real JDK bytecode runs, so
/// these wrappers are not reachable at all.
fn plain_server_socket_delegate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    pick: fn(
        &cratonvm_native_api::plain_server_socket::PlainServerSocketOps,
    ) -> cratonvm_native_api::registry::NativeCallback,
    fallback: impl FnOnce(&mut dyn NativeContext) -> MethodCallResult,
) -> MethodCallResult {
    match cratonvm_native_api::plain_server_socket::get() {
        Some(ops) => pick(&ops)(ctx, args),
        None => fallback(ctx),
    }
}

fn ss_wrapper_local_port(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    if let Some(ssc) = ss_back_ref(ctx, this) {
        let port = cf_get(ctx, ssc, F_LOCAL_PORT).as_int().unwrap_or(0);
        return Ok(Some(Value::Int(port)));
    }
    // No ServerSocketChannel back-ref: this is a PLAIN synthetic `java.net.ServerSocket`
    // (bound by the net_phase_e re2 path), not a channel adapter. This native is the
    // last-registered `getLocalPort` and therefore shadows the plain ServerSocket too,
    // so answering here breaks every plain-socket caller that reads its bound port
    // (e.g. Narayana's TransactionStatusManager advertises getLocalPort() and its
    // recovery connector then connects to it → the Hibernate JTA cluster hang).
    // Only the owner knows whether the socket was ever bound (-1) versus bound and
    // since closed (still the port), so ask it rather than reconstructing an answer
    // from the port side table.
    plain_server_socket_delegate(ctx, args, |ops| ops.local_port, |ctx| {
        let p = cratonvm_native_api::server_socket_ports::get(ctx.identity_hash_code(this), this)
            .unwrap_or(0);
        Ok(Some(Value::Int(p)))
    })
}

fn ss_wrapper_local_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let Some(ssc) = ss_back_ref(ctx, this) else {
        // Plain ServerSocket — only the owning crate knows whether this socket
        // was ever bound (null) versus bound and since closed (still its
        // address); see `ss_wrapper_local_port`. The `server_socket_ports`
        // side table is the fallback for a VM with no handler set installed.
        let identity = ctx.identity_hash_code(this);
        return plain_server_socket_delegate(ctx, args, |ops| ops.local_socket_address, |ctx| {
            match cratonvm_native_api::server_socket_ports::get_addr(identity, this) {
                Some((host, port)) if port > 0 => new_resolved_inet_socket_address(ctx, &host, port),
                _ => Ok(Some(Value::Object(None))),
            }
        });
    };
    let (port, host) = {
        let port = cf_get(ctx, ssc, F_LOCAL_PORT).as_int().unwrap_or(0);
        let id = cf_get(ctx, ssc, F_REG_ID).as_int().unwrap_or(-1);
        // Real bound address, not the historical "0.0.0.0" placeholder —
        // see `ssc_local_address` (same registry, same rationale). Also
        // route through `advertised_listener_host` like `ssc_local_address`
        // does: a wildcard listener is a valid bind target but not a valid
        // client connect destination on Windows (WSAEADDRNOTAVAIL / os error
        // 10049). Jetty's `ServerConnector` reads this via
        // `ServerSocketChannel.socket().getLocalSocketAddress()` to publish
        // the host its own reactor-netty-backed test client connects to, so
        // this path needs the same loopback substitution the RSocket fix
        // applied to `ssc_local_address`.
        let host = match tcp_registry().read().get(&id) {
            Some(TcpHandle::Listener(l)) => l
                .local_addr()
                .map(advertised_listener_host)
                .unwrap_or_else(|_| "0.0.0.0".to_string()),
            _ => "0.0.0.0".to_string(),
        };
        (port, host)
    };
    if port <= 0 {
        return Ok(Some(Value::Object(None)));
    }
    // Build via the REAL `InetSocketAddress(InetAddress,int)` ctor (like
    // `ssc_local_address` above), NOT a flat 2-slot synthetic: the real
    // `getPort()`/`getHostString()`/`toString()` bytecode reads
    // `this.holder.port` / `this.holder.hostname`, and a flat object has a null
    // `holder` → `getPort()` returns 0. okhttp's `MockWebServer.getPort()` reads
    // `(serverSocket.localSocketAddress as InetSocketAddress).port`, so the flat
    // object made it 0 → every Spring HTTP-client test connected to
    // `http://localhost:0` and failed (BUG-04). The resolved real ctor also
    // populates holder.addr, which WildFly's process controller dereferences.
    new_resolved_inet_socket_address(ctx, &host, port)
}

fn ss_wrapper_is_bound(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let Some(ssc) = ss_back_ref(ctx, this) else {
        // Plain ServerSocket — `isBound()` stays true after close(), which the
        // bound-port side table cannot express (close() removes the entry), so
        // ask the owner.
        let bound =
            cratonvm_native_api::server_socket_ports::get(ctx.identity_hash_code(this), this)
                .is_some();
        return plain_server_socket_delegate(
            ctx,
            args,
            |ops| ops.is_bound,
            |_| Ok(Some(Value::Int(i32::from(bound)))),
        );
    };
    let id = cf_get(ctx, ssc, F_REG_ID).as_int().unwrap_or(-1);
    Ok(Some(Value::Int(if id >= 0 { 1 } else { 0 })))
}

fn ss_wrapper_is_closed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(1))),
    };
    if let Some(ssc) = ss_back_ref(ctx, this) {
        let open = cf_get(ctx, ssc, F_OPEN).as_int().unwrap_or(0);
        return Ok(Some(Value::Int(if open == 0 { 1 } else { 0 })));
    }
    // Plain ServerSocket. This used to answer a hardcoded `false`: nothing in
    // native-io tracks the closed state, so `isClosed()` was permanently false
    // even right after `close()` — a lifecycle check every server loop makes.
    plain_server_socket_delegate(ctx, args, |ops| ops.is_closed, |_| Ok(Some(Value::Int(0))))
}

fn ss_wrapper_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if let Some(ssc) = ss_back_ref(ctx, this) {
        let _ = ssc_close(ctx, &[Value::Object(Some(ssc))])?;
        // Remove only this receiver's collision bucket entry.
        ss_remove_back_ref(ctx, this);
        return Ok(None);
    }
    // Plain ServerSocket (no ServerSocketChannel back-ref). This native is the
    // last-registered — and therefore winning — `close`, but the listener it
    // must drop lives in native-builtins' `s2` registry (the binding went
    // through the plain-bind handler). Delegate to the matching close handler;
    // previously this no-opped, so the listener stayed registered and a thread
    // blocked in ServerSocket.accept() never woke — okhttp's
    // MockWebServer.close() then threw `AssertionError: Gave up waiting for
    // queue to shut down` on teardown.
    plain_server_socket_delegate(ctx, args, |ops| ops.close, |_| Ok(None))
}

/// `ServerSocket.accept()` for a `ServerSocketChannel.socket()` adapter.
///
/// Installed into `cratonvm_native_api::plain_server_socket` and called from
/// net_phase_e's RE.2 `accept` native, which wins that triple but only
/// understands plain sockets. `None` means "no channel back-ref, not mine".
///
/// Without this an adapter's `accept()` reported
/// `IOException: ServerSocket not bound`: its listener is in THIS crate's
/// channel registry, so RE.2 saw `listener_id = -1` and concluded the socket
/// had never been bound. `timeout_ms` is the SO_TIMEOUT RE.2 holds for the
/// adapter (it owns the unwrapped `setSoTimeout` too); `0` blocks
/// indefinitely, matching `ServerSocketAdaptor.accept`.
fn ss_adapter_channel_accept(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    timeout_ms: i32,
) -> Option<MethodCallResult> {
    let ssc = ss_back_ref(ctx, this)?;
    let deadline = (timeout_ms > 0)
        .then(|| std::time::Instant::now() + Duration::from_millis(timeout_ms as u64));
    Some(ss_adapter_accept_on(ctx, ssc, deadline))
}

/// `SocketChannelImpl.blockingRead(byte[] b, int off, int len, long nanos)`.
///
/// The read side of the `Socket` a channel adapter hands out:
/// `SocketAdaptor.getInputStream()` returns a `sun.nio.ch.SocketInputStream`,
/// whose `implRead` calls this. It is declared on `SocketChannelImpl`, but
/// CratonVM's `SocketChannel.open()` (and the channel produced by an accept)
/// is an instance of the ABSTRACT `java.nio.channels.SocketChannel`, so the
/// call landed on a receiver without the method:
/// `NoSuchMethodError: java.nio.channels.SocketChannel.blockingRead([BIIJ)I`.
/// Same shape as `blockingAccept` on the server side.
///
/// `nanos <= 0` means block indefinitely; a positive deadline that expires
/// throws `SocketTimeoutException`, which is how `Socket.setSoTimeout` reaches
/// a read on this path.
fn sc_blocking_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_or_none(args, 0).ok_or_else(|| ioex("blockingRead: null channel"))?;
    let arr = obj_or_none(args, 1).ok_or_else(|| ioex("blockingRead: null buffer"))?;
    let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let nanos = match args.get(4) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => i64::from(*n),
        _ => 0,
    };
    if len == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let id = read_reg_id(ctx, this).ok_or_else(|| ioex("blockingRead: channel not connected"))?;
    let deadline =
        (nanos > 0).then(|| std::time::Instant::now() + Duration::from_nanos(nanos as u64));

    let mut buf = vec![0u8; len];
    let arr_pin = ctx.pin_native_root(arr);
    ctx.begin_blocking_region();
    let outcome = loop {
        match resolve_stream(id) {
            StreamTarget::Ready(stream) => match try_read_nb(&stream, &mut buf) {
                Ok(Some(n)) => break Ok(n),
                Ok(None) => {}
                Err(error) => break Err(map_err("blockingRead", error)),
            },
            StreamTarget::Connecting => {}
            StreamTarget::Failed(error) => break Err(map_err("blockingRead", error)),
            StreamTarget::Unavailable => break Err(ioex("blockingRead: channel not a stream")),
        }
        if let Some(deadline) = deadline {
            if std::time::Instant::now() >= deadline {
                break Err(RuntimeError::SocketTimeoutException {
                    message: "Read timed out".into(),
                }
                .into());
            }
        }
        std::thread::sleep(ADAPTER_READ_POLL);
    };
    ctx.end_blocking_region();

    let n = match outcome {
        Ok(n) => n,
        Err(failed) => {
            ctx.unpin_native_roots(arr_pin);
            return Err(failed);
        }
    };
    if n > 0 {
        crate::net::socket_capture('r', id, &buf[..n as usize]);
        // Reload through the pin: the wait above may have crossed a GC pause
        // that relocated the array.
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.write_byte_array_from(arr, off, &buf[..n as usize]);
    }
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Int(n)))
}

/// `SocketChannelImpl.blockingWriteFully(byte[] b, int off, int len)` — the
/// write side of the same adapter stream. Writes every byte or throws; the
/// JDK's `SocketOutputStream.implWrite` relies on "fully".
fn sc_blocking_write_fully(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_or_none(args, 0).ok_or_else(|| ioex("blockingWriteFully: null channel"))?;
    let arr = obj_or_none(args, 1).ok_or_else(|| ioex("blockingWriteFully: null buffer"))?;
    let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    if len == 0 {
        return Ok(None);
    }
    let id =
        read_reg_id(ctx, this).ok_or_else(|| ioex("blockingWriteFully: channel not connected"))?;
    let mut data = vec![0u8; len];
    let copied = ctx.read_byte_array_into(arr, off, &mut data);
    data.truncate(copied);
    if data.is_empty() {
        return Ok(None);
    }

    ctx.begin_blocking_region();
    let mut written = 0usize;
    let result = loop {
        if written == data.len() {
            break Ok(());
        }
        match resolve_stream(id) {
            StreamTarget::Ready(stream) => match try_write_nb(&stream, &data[written..]) {
                Ok(Some(n)) if n > 0 => {
                    written += n as usize;
                    continue;
                }
                // Zero-length or WouldBlock: back off and retry, since this
                // method must not return short.
                Ok(_) => {}
                Err(error) => break Err(map_err("blockingWriteFully", error)),
            },
            StreamTarget::Connecting => {}
            StreamTarget::Failed(error) => break Err(map_err("blockingWriteFully", error)),
            StreamTarget::Unavailable => {
                break Err(ioex("blockingWriteFully: channel not a stream"))
            }
        }
        std::thread::sleep(ADAPTER_READ_POLL);
    };
    ctx.end_blocking_region();
    result?;
    crate::net::socket_capture('w', id, &data);
    Ok(None)
}

/// Back-off between attempts while an adapter read/write waits for the socket.
/// Matches the accept loop's cadence — these paths are only reached by the
/// `Socket` view of a channel, never by the hot NIO path.
const ADAPTER_READ_POLL: Duration = Duration::from_millis(5);

fn ss_adapter_accept_on(
    ctx: &mut dyn NativeContext,
    ssc: ObjectRef,
    deadline: Option<std::time::Instant>,
) -> MethodCallResult {
    // The same primitive `blockingAccept(nanos)` uses: a bounded wait throws
    // SocketTimeoutException on expiry rather than returning null.
    let accepted = ssc_accept_impl(ctx, &[Value::Object(Some(ssc))], deadline)?;
    let Some(Value::Object(Some(channel))) = accepted else {
        // Only reachable with no deadline on a NON-blocking channel, where
        // `ServerSocketChannel.accept()` legitimately answers null. The real
        // `ServerSocketAdaptor.accept` refuses that configuration outright
        // rather than hand back a null Socket for its caller to dereference.
        return Err(ioex(
            "accept: ServerSocket.accept() on a non-blocking ServerSocketChannel \
             with no connection pending",
        ));
    };
    // `ServerSocket.accept()` returns a `java.net.Socket`, not a SocketChannel.
    sc_socket(ctx, &[Value::Object(Some(channel))])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use std::io::{Read as _, Write as _};

    #[test]
    fn advertised_listener_host_converts_only_wildcard_listener_addresses() {
        assert_eq!(
            advertised_listener_host("0.0.0.0:49152".parse().unwrap()),
            "127.0.0.1"
        );
        assert_eq!(
            advertised_listener_host("[::]:49152".parse().unwrap()),
            "::1"
        );
        assert_eq!(
            advertised_listener_host("127.0.0.2:49152".parse().unwrap()),
            "127.0.0.2"
        );
    }

    /// `RJdkNio.selectorAndAsyncClose` at the Rust layer: a reader parked on a
    /// blocking channel must break out when another thread closes it.
    /// `sc_close`'s `shutdown(Write)` + `tcp_remove` cannot reach a parked
    /// `recv` — this reader holds an `Arc` clone of the very stream the map
    /// dropped, so the OS handle stays open — which is why the wakeup has to
    /// come from the registry check rather than from the socket.
    #[test]
    fn a_close_breaks_a_parked_blocking_channel_read() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Hold the accepted end open and silent so the reader genuinely parks.
        let keeper = std::thread::spawn(move || listener.accept().unwrap().0);

        let client = Arc::new(TcpStream::connect(("127.0.0.1", port)).unwrap());
        let id = tcp_register(TcpHandle::Stream(Arc::clone(&client)));

        let reader = std::thread::spawn(move || {
            let mut buf = [0_u8; 16];
            let start = std::time::Instant::now();
            let outcome = read_close_aware(id, &client, &mut buf);
            (outcome.map_err(|e| e.kind()), start.elapsed())
        });

        std::thread::sleep(Duration::from_millis(100));
        tcp_remove(id);

        let (outcome, elapsed) = reader.join().unwrap();
        assert_eq!(
            outcome.unwrap_err(),
            ErrorKind::Interrupted,
            "an asynchronous close must break the parked read"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "close should wake the parked reader promptly, got {elapsed:?}"
        );
        drop(keeper.join().unwrap());
    }

    /// The other half of the contract: a close-aware read is still a read.
    /// Bytes that arrive while the reader is parked come back on the next pass,
    /// not after a poll timeout.
    #[test]
    fn a_close_aware_read_still_delivers_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let writer = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(100));
            peer.write_all(b"ping").unwrap();
            peer
        });

        let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let id = tcp_register(TcpHandle::Stream(Arc::new(client.try_clone().unwrap())));
        let mut buf = [0_u8; 16];
        let n = read_close_aware(id, &client, &mut buf)
            .expect("read must succeed")
            .expect("a blocking close-aware read never answers UNAVAILABLE");
        assert_eq!(&buf[..n as usize], b"ping");
        tcp_remove(id);
        drop(writer.join().unwrap());
    }

    /// Regression guard for the Jetty `givenAnInflightRequestWhenTheServerIs
    /// StoppedThenGracefulShutdownCallbackIsCalledWithRequestsActive` hang
    /// (`jetty-webserver-factory-poststartup-timeout-and-reflective-
    /// supertype-residuals.md`): `AbstractReactiveWebServerFactoryTests`
    /// builds its client target via `new InetSocketAddress(port)`, which
    /// produces a wildcard host. Connecting to that host verbatim throws
    /// WSAEADDRNOTAVAIL on Windows instead of reaching the server, leaving
    /// the test's `BlockingHandler.awaitQueue()` parked forever.
    #[test]
    fn connect_target_host_substitutes_loopback_for_wildcard_only() {
        assert_eq!(connect_target_host("0.0.0.0".to_string()), "127.0.0.1");
        assert_eq!(connect_target_host("::".to_string()), "::1");
        assert_eq!(connect_target_host("0:0:0:0:0:0:0:0".to_string()), "::1");
        assert_eq!(connect_target_host("127.0.0.2".to_string()), "127.0.0.2");
        assert_eq!(
            connect_target_host("example.invalid".to_string()),
            "example.invalid"
        );
    }

    #[test]
    fn registers_without_panic() {
        let mut r = NativeMethodRegistry::new();
        register_socket_channel_real(&mut r);
        // Sanity: the most-used methods are reachable.
        assert!(r
            .find(
                "java/nio/channels/SocketChannel",
                "open",
                "()Ljava/nio/channels/SocketChannel;"
            )
            .is_some());
        assert!(r
            .find(
                "sun/nio/ch/SocketChannelImpl",
                "configureBlocking",
                "(Z)Ljava/nio/channels/SelectableChannel;"
            )
            .is_some());
        assert!(r
            .find(
                "java/nio/channels/SocketChannel",
                "bind",
                "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;"
            )
            .is_some());
        assert!(r
            .find(
                "java/nio/channels/ServerSocketChannel",
                "accept",
                "()Ljava/nio/channels/SocketChannel;"
            )
            .is_some());
    }

    #[test]
    fn blocking_accept_observes_channel_close_promptly() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let id = tcp_register(TcpHandle::Listener(listener));

        let waiter = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let err = accept_close_aware(id, true).unwrap_err();
            (err.kind(), start.elapsed())
        });

        std::thread::sleep(Duration::from_millis(50));
        tcp_remove(id);
        let (kind, elapsed) = waiter.join().unwrap();
        assert_eq!(kind, ErrorKind::Interrupted);
        assert!(
            elapsed < Duration::from_secs(2),
            "close-aware accept should wake promptly, got {elapsed:?}"
        );
    }

    /// Dropping the registry entry must close the OS socket, even with an
    /// acceptor thread parked in `accept_close_aware`.
    ///
    /// The registry is the sole owner of the listening socket, so
    /// `tcp_remove` — which is what `ServerSocketChannel.close()` ends in —
    /// closes the port. An acceptor that kept its own `try_clone()`d duplicate
    /// silently defeated that: the port stayed open for up to one
    /// `ACCEPT_CLOSE_POLL`, and a connection arriving in that window was
    /// accepted and served. Spring Boot's `JettyServletWebServerFactoryTests
    /// .whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade` saw
    /// that as `404 Not Found` where it expected a refused connection.
    ///
    /// Assert the OS-visible property directly — a connect to the port has to
    /// fail — rather than the internal one, because the internal state was
    /// already correct while the socket stayed open.
    #[test]
    fn closing_the_registry_entry_closes_the_listening_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let id = tcp_register(TcpHandle::Listener(listener));

        let acceptor = std::thread::spawn(move || accept_close_aware(id, true).map(|_| ()));
        // Let the acceptor reach its first poll, so the close lands with a
        // thread actively accepting.
        std::thread::sleep(Duration::from_millis(50));

        tcp_remove(id);
        assert_eq!(
            acceptor.join().unwrap().unwrap_err().kind(),
            ErrorKind::Interrupted
        );

        let target: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let connected =
            std::net::TcpStream::connect_timeout(&target, Duration::from_millis(250)).is_ok();
        assert!(
            !connected,
            "port {port} still accepted a connection after its registry entry \
             was dropped — something outlived the close"
        );
    }

    #[test]
    fn h3b_resolve_and_vet_blocks_link_local_literal() {
        // Direct link-local IP must be denied before any dial.
        crate::outbound_policy::reset_policy();
        assert!(
            resolve_and_vet("169.254.169.254:80").is_err(),
            "expected AWS IMDS literal to be denied"
        );
        assert!(
            resolve_and_vet("169.254.170.2:80").is_err(),
            "expected 169.254.0.0/16 neighbour to be denied"
        );
        assert!(
            resolve_and_vet("[fd00:ec2::254]:80").is_err(),
            "expected IPv6 AWS metadata to be denied"
        );
    }

    #[test]
    fn h3b_resolve_and_vet_allows_and_resolves_loopback() {
        // Loopback resolves and passes the policy; the returned addrs are
        // concrete literals (no hostname left to re-resolve downstream).
        crate::outbound_policy::reset_policy();
        let addrs = resolve_and_vet("127.0.0.1:9").expect("loopback should be allowed");
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.ip().is_loopback()));
    }

    #[test]
    fn nb_read_returns_eagain_zero() {
        // Spawn a real listener that just accepts and never sends anything.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _t = std::thread::spawn(move || {
            let _ = listener.accept();
            std::thread::sleep(Duration::from_secs(2));
        });
        // Connect and switch to non-blocking.
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 16];
        let res = try_read_nb(&stream, &mut buf).unwrap();
        // No data has been sent — EAGAIN translates to None.
        assert!(res.is_none());
    }

    #[test]
    fn nb_write_partial() {
        // A round-trip on a small buffer: write a few bytes and read them back.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 16];
            let n = s.read(&mut buf).unwrap();
            buf[..n].to_vec()
        });
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_nonblocking(true).unwrap();
        let n = try_write_nb(&stream, b"hello").unwrap().unwrap_or(0);
        assert!(n > 0);
        // Block on the server side to confirm bytes arrived.
        let got = server.join().unwrap();
        assert_eq!(got.as_slice(), &b"hello"[..n as usize]);
    }

    #[test]
    fn tomcat0807_http_lingering_channel_close_sends_write_fin_not_full_reset() {
        // sc_close no longer starts a background drain thread (2026-07-11 --
        // see task #11 in the swallow-uploads known-issue doc: an
        // unconditional background drain masked Tomcat's own
        // checkSwallowInput()/DISABLE_SWALLOW_INPUT intent, silently turning
        // every intentional abort-without-swallow into a graceful close).
        // What lingering_channel_close still must guarantee is the original,
        // narrower concern: a write-side FIN so a peer's blocking read sees
        // EOF instead of hanging forever (the selector-duplicate-handle
        // issue documented on sc_close itself) -- not that arbitrary-sized
        // peer uploads always complete without a reset.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            lingering_channel_close(0x0807, &stream);
            drop(stream);
        });

        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        server.join().unwrap();

        // The peer's read must see a clean EOF (not hang, not error) once
        // the server's write-side FIN arrives.
        let mut buf = [0u8; 16];
        let n = client.read(&mut buf).unwrap();
        assert_eq!(n, 0, "peer read should observe EOF after write-shutdown");
    }
}
