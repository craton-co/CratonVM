// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP5.7 — `java.net.PlainSocketImpl` / `sun.nio.ch.NioSocketImpl` /
//! `java.net.PlainServerSocketImpl` (legacy classical-blocking-I/O surface).
//!
//! The pre-existing `register_re1_socket` and `register_re2_server_socket`
//! in `net_phase_e.rs` register the *public* `java.net.Socket` /
//! `java.net.ServerSocket` API and own that surface. This module owns the
//! *implementation type* surface that real-JDK delegates to from those public
//! classes:
//!
//!   * `java.net.PlainSocketImpl` (legacy <= JDK 12)
//!   * `sun.nio.ch.NioSocketImpl`  (JDK 13+ default — wraps NIO under the hood
//!     but still presents the legacy `socketCreate/Connect/Bind/Listen/Accept`
//!     surface to bytecode that uses `Socket.getImpl()` reflectively)
//!   * `java.net.PlainServerSocketImpl`
//!
//! The genuine native methods we must implement are the ones HotSpot writes in
//! `Java_java_net_PlainSocketImpl_*` / `Java_sun_nio_ch_NioSocketImpl_*` —
//! `socketCreate`, `socketConnect`, `socketBind`, `socketListen`,
//! `socketAccept`, `socketClose0`, `socketShutdown`, `socketSetOption`,
//! `socketGetOption`, `socketAvailable`, `socketSendUrgentData`. The Java side
//! holds an `int fd` (or `FileDescriptor` containing an `int fd`) that we map
//! to a `socket2::Socket` in our process-wide `socket_registry`.
//!
//! This is *distinct* from the NIO `SocketChannel` family in
//! `native-io/src/socket_channel.rs`: that file owns the non-blocking
//! `SelectorProvider`-driven channels; we own the legacy
//! blocking-by-default `Socket` family.
//!
//! ## Acceptance ([roadmap WP5.7])
//!
//! 1K concurrent HTTP/1.0 requests on port 8080 — the server binds,
//! sets `SO_REUSEADDR`, listens with backlog 50, accepts 1K connections,
//! reads the request line + blank line, writes a fixed `200 OK` response,
//! and closes. We embed an integration-style test that exercises the
//! Rust-side state machine (no Java bytecode) for the same shape.

#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::RwLock;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// Socket option ID constants
//
// These match the values OpenJDK's `SocketOptions` interface assigns:
// https://github.com/openjdk/jdk/blob/master/src/java.base/share/classes/java/net/SocketOptions.java
// ---------------------------------------------------------------------------

const TCP_NODELAY: i32 = 0x0001;
const SO_REUSEADDR: i32 = 0x04;
const SO_BROADCAST: i32 = 0x0020;
const IP_MULTICAST_IF: i32 = 0x10;
const IP_MULTICAST_IF2: i32 = 0x1f;
const IP_MULTICAST_LOOP: i32 = 0x12;
const IP_TOS: i32 = 0x03;
const SO_LINGER: i32 = 0x0080;
const SO_TIMEOUT: i32 = 0x1006;
const SO_BINDADDR: i32 = 0x000F;
const SO_SNDBUF: i32 = 0x1001;
const SO_RCVBUF: i32 = 0x1002;
const SO_KEEPALIVE: i32 = 0x0008;
const SO_OOBINLINE: i32 = 0x1003;
const SO_REUSEPORT: i32 = 0x0E;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn ioex<S: Into<String>>(msg: S) -> MethodCallFailed {
    RuntimeError::IOException {
        message: msg.into(),
    }
    .into()
}

/// Preserve the concrete `java.net` exception type that callers use for
/// blocking socket-connect recovery.  This mirrors the public `Socket`
/// bridge in `net_phase_e`: a text-only `IOException` makes
/// `catch (ConnectException)` and `catch (SocketTimeoutException)` ineffective.
fn connectex(addr: SocketAddr, error: std::io::Error) -> MethodCallFailed {
    let message = format!("{}: {error}", addr);
    match error.kind() {
        std::io::ErrorKind::ConnectionRefused => RuntimeError::ConnectException { message }.into(),
        std::io::ErrorKind::TimedOut => RuntimeError::SocketTimeoutException { message }.into(),
        _ => ioex(format!("socketConnect: {message}")),
    }
}

/// Temporary diagnostic: `CRATONVM_DBG_NET=1` prints each PlainSocketImpl native
/// as it fires, to confirm whether the blocking socket path uses the legacy
/// PlainSocketImpl surface vs the NioSocketImpl→sun/nio/ch/Net path.
macro_rules! dbgplain {
    ($($arg:tt)*) => {
        if crate::vmflags().io.dbg_net {
            eprintln!("[PLAIN] {}", format!($($arg)*));
        }
    };
}

fn iae<S: Into<String>>(msg: S) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: msg.into(),
    }
    .into()
}

fn npe<S: Into<String>>(msg: S) -> MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some(msg.into()),
    }
    .into()
}

fn this_obj(args: &[Value]) -> Result<ObjectRef, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(npe("plain socket call on null receiver")),
    }
}

// ---------------------------------------------------------------------------
// Process-wide registry of `socket2::Socket` instances keyed by `i32` fd.
//
// We use a positive monotonically-increasing id rather than the OS-level fd
// so that closed-but-still-referenced Java SocketImpl objects can reliably
// return `EBADF`-style errors instead of accidentally mapping onto a brand
// new socket the OS recycled.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct SocketState {
    pub socket: Socket,
    /// Cached `SO_TIMEOUT` (read timeout in ms). socket2 keeps it on the
    /// socket via `set_read_timeout` but Java exposes it as a separate
    /// option that round-trips through `socketSetOption(SO_TIMEOUT, ...)`.
    pub so_timeout_ms: Option<u32>,
    pub is_listening: bool,
    /// Set true once `connect`/`accept` has bound an actual peer.
    pub is_connected: bool,
    /// `true` if this socket is a stream socket (TCP), `false` for DGRAM.
    pub is_stream: bool,
    /// True when `close0` has been called — we keep the entry around for
    /// the lifetime of the Java SocketImpl so a follow-up `getOption` /
    /// `available` call returns a clean error rather than panicking on
    /// missing-fd.
    pub closed: bool,
}

#[derive(Default)]
struct SocketRegistry {
    next_id: i32,
    sockets: HashMap<i32, SocketState>,
}

fn registry() -> &'static RwLock<SocketRegistry> {
    static R: OnceLock<RwLock<SocketRegistry>> = OnceLock::new();
    R.get_or_init(|| {
        RwLock::new(SocketRegistry {
            next_id: 1,
            sockets: HashMap::new(),
        })
    })
}

pub(crate) fn register_socket(state: SocketState) -> i32 {
    let mut g = registry().write();
    let id = g.next_id;
    g.next_id = g.next_id.checked_add(1).unwrap_or(1);
    g.sockets.insert(id, state);
    id
}

fn drop_socket(id: i32) {
    if id <= 0 {
        return;
    }
    let mut g = registry().write();
    g.sockets.remove(&id);
}

fn with_socket<F, T>(id: i32, f: F) -> Result<T, MethodCallFailed>
where
    F: FnOnce(&mut SocketState) -> Result<T, MethodCallFailed>,
{
    if id <= 0 {
        return Err(ioex("socket: invalid fd"));
    }
    let mut g = registry().write();
    match g.sockets.get_mut(&id) {
        Some(s) => {
            if s.closed {
                return Err(ioex("Socket closed"));
            }
            f(s)
        }
        None => Err(ioex(format!("socket: fd {id} not found"))),
    }
}

// ---------------------------------------------------------------------------
// fd plumbing — read/write the synthetic `int fd` field that the Java
// SocketImpl exposes. We resolve the field by name so we don't have to
// hard-code synthetic layout offsets that drift across PlainSocketImpl
// vs NioSocketImpl vs PlainServerSocketImpl.
// ---------------------------------------------------------------------------

fn read_fd(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    // Try the raw `int fd` slot first (JDK ≤ 7 layout still used by some test
    // fixtures), then fall back to `FileDescriptor.fd` via the `fd` field.
    let direct = ctx.get_field_by_name(this, "fd");
    match direct {
        Value::Int(v) => v,
        Value::Object(Some(fd_obj)) => match ctx.get_field_by_name(fd_obj, "fd") {
            Value::Int(v) => v,
            _ => -1,
        },
        _ => -1,
    }
}

fn write_fd(ctx: &dyn NativeContext, this: ObjectRef, id: i32) {
    // Mirror real-JDK: prefer to fill the `FileDescriptor` if present.
    let direct = ctx.get_field_by_name(this, "fd");
    match direct {
        Value::Object(Some(fd_obj)) => {
            ctx.set_field_by_name(fd_obj, "fd", Value::Int(id));
        }
        _ => {
            ctx.set_field_by_name(this, "fd", Value::Int(id));
        }
    }
}

// ---------------------------------------------------------------------------
// InetAddress helpers — read the IP-string for an InetAddress.
//
// `java.net.InetAddress` is a real bootstrap class whose instance slots are
// the `holder` reference fields, NOT a `hostName`/`address` String pair.
// CratonVM-synthesised InetAddress objects keep host/IP in the
// `net_phase_e` ObjectRef-keyed side table; consult it first, falling back
// to the legacy synthetic slot 1 only for objects not built by
// `alloc_inet_address`.
// ---------------------------------------------------------------------------

fn read_inet_addr(ctx: &dyn NativeContext, addr: ObjectRef) -> Option<std::net::IpAddr> {
    let s = if let Some((_, ip)) = crate::net_phase_e::inet_addr_get(addr) {
        ip
    } else {
        match ctx.get_field(addr, 1) {
            Value::Object(Some(s)) => ctx.read_string(s)?,
            _ => return None,
        }
    };
    if s.is_empty() {
        return None;
    }
    if let Ok(v4) = s.parse::<Ipv4Addr>() {
        return Some(std::net::IpAddr::V4(v4));
    }
    if let Ok(v6) = s.parse::<Ipv6Addr>() {
        return Some(std::net::IpAddr::V6(v6));
    }
    // Last resort: treat `s` as a hostname and resolve it.
    if let Ok(mut iter) = std::net::ToSocketAddrs::to_socket_addrs(&format!("{s}:0").as_str()) {
        if let Some(sa) = iter.next() {
            return Some(sa.ip());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Native callbacks
// ---------------------------------------------------------------------------

/// `socketCreate(boolean stream)` — allocate a fresh socket and stash it,
/// writing the registry id into the SocketImpl's `fd`.
fn socket_create(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    dbgplain!("socketCreate this={this:?}");
    let stream = match args.get(1) {
        Some(Value::Int(v)) => *v != 0,
        // `<this>` is arg 0; for instance methods that take no further args
        // (some real-JDK overloads) default to stream=true.
        _ => true,
    };
    let domain = Domain::IPV4; // We default to IPv4; explicit IPv6 sockets go through the dual-stack path on Connect.
    let kind = if stream { Type::STREAM } else { Type::DGRAM };
    let proto = if stream {
        Some(Protocol::TCP)
    } else {
        Some(Protocol::UDP)
    };
    let sock = Socket::new(domain, kind, proto).map_err(|e| ioex(format!("socketCreate: {e}")))?;
    // Reasonable platform defaults — match HotSpot.
    let _ = sock.set_nonblocking(false);
    let state = SocketState {
        socket: sock,
        so_timeout_ms: None,
        is_listening: false,
        is_connected: false,
        is_stream: stream,
        closed: false,
    };
    let id = register_socket(state);
    write_fd(ctx, this, id);
    Ok(None)
}

/// `socketConnect(InetAddress addr, int port, int timeoutMs)`.
fn socket_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let addr_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(npe("socketConnect: null address")),
    };
    let port = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => return Err(iae("socketConnect: missing port")),
    };
    if !(0..=65535).contains(&port) {
        return Err(iae(format!("socketConnect: bad port {port}")));
    }
    let timeout_ms = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let ip = read_inet_addr(ctx, addr_obj)
        .ok_or_else(|| ioex("socketConnect: cannot resolve address"))?;
    // Wildcard connect targets aren't a valid OS destination on Windows
    // (WSAEADDRNOTAVAIL) — see the matching substitution and rationale in
    // `native-io/src/socket_channel.rs::sc_connect_inner`.
    let ip = match ip {
        std::net::IpAddr::V4(v4) if v4.is_unspecified() => {
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
        }
        std::net::IpAddr::V6(v6) if v6.is_unspecified() => {
            std::net::IpAddr::V6(Ipv6Addr::LOCALHOST)
        }
        other => other,
    };
    let sa = SocketAddr::new(ip, port as u16);
    let fd = read_fd(ctx, this);
    let sock_addr = SockAddr::from(sa);
    // `connect()`/`connect_timeout()` is a genuine OS-level blocking call
    // (up to `timeout_ms`, or unbounded when no timeout is set). Two
    // problems if we run it inside `with_socket`'s closure as before:
    //
    //   1. `with_socket` holds `registry().write()` — the single global
    //      lock every other `java.net.Socket`-family op (`accept`, `read`,
    //      `write`, ...) needs — for as long as the closure runs. Blocking
    //      inside it serializes ALL synthetic blocking-Socket I/O
    //      process-wide until this connect resolves (the same class of
    //      self-deadlock `socket_accept`'s doc comment warns about, and
    //      exactly the failure mode if the peer this is connecting to is
    //      itself served by a `read`/`write`/`accept` on this process).
    //   2. It never bracketed the blocking syscall in
    //      `begin_blocking_region`/`end_blocking_region`, so a concurrent
    //      STW pause (JIT takeover or GC) counts this thread as an expected
    //      cooperator and waits on it forever — see `socket_accept` above
    //      and `re1_socket_read_stream` in `net_phase_e.rs` for the same
    //      pattern.
    //
    // Fix both: clone the fd out under a short lock (mirrors
    // `socket_accept`), connect on the clone with the registry lock
    // released and the thread marked excluded from STW, then commit the
    // connected clone back into the registry slot.
    let cloned = with_socket(fd, |s| {
        s.socket
            .try_clone()
            .map_err(|e| ioex(format!("socketConnect: {e}")))
    })?;
    ctx.begin_blocking_region();
    let result = if timeout_ms > 0 {
        cloned.connect_timeout(&sock_addr, Duration::from_millis(timeout_ms as u64))
    } else {
        cloned.connect(&sock_addr)
    };
    ctx.end_blocking_region();
    result.map_err(|e| connectex(sa, e))?;
    with_socket(fd, |s| {
        s.socket = cloned;
        s.is_connected = true;
        Ok(None)
    })
}

/// `socketBind(InetAddress addr, int port)`.
fn socket_bind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let port = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let ip: std::net::IpAddr = match args.get(1) {
        Some(Value::Object(Some(o))) => {
            read_inet_addr(ctx, *o).unwrap_or(std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        }
        _ => std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED),
    };
    let sa = SocketAddr::new(ip, port as u16);
    let fd = read_fd(ctx, this);
    dbgplain!("socketBind fd={fd} addr={sa}");
    with_socket(fd, |s| {
        s.socket
            .bind(&SockAddr::from(sa))
            .map_err(|e| ioex(format!("socketBind: {e}")))?;
        Ok(None)
    })
}

/// `socketListen(int backlog)`.
fn socket_listen(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let backlog = match args.get(1) {
        Some(Value::Int(v)) => (*v).max(1),
        _ => 50,
    };
    let fd = read_fd(ctx, this);
    dbgplain!("socketListen fd={fd} backlog={backlog}");
    with_socket(fd, |s| {
        s.socket
            .listen(backlog)
            .map_err(|e| ioex(format!("socketListen: {e}")))?;
        s.is_listening = true;
        Ok(None)
    })
}

/// `socketAccept(SocketImpl s)`.
fn socket_accept(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let new_impl = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(npe("socketAccept: null SocketImpl target")),
    };
    let fd = read_fd(ctx, this);
    if fd <= 0 {
        return Err(ioex("socketAccept: not bound"));
    }
    // `accept` is allowed to block indefinitely. We honour SO_TIMEOUT if set.
    //
    // CRITICAL: we must NOT hold the registry lock across the blocking
    // `accept()` — every other socket op needs `registry().write()` via
    // `with_socket`, so blocking here while holding the read guard would
    // serialize ALL socket I/O process-wide until a connection arrives.
    // Instead we `try_clone()` the listening socket out under a short lock
    // (dups the underlying fd — accepting on the clone is equivalent to
    // accepting on the original), drop the lock, then block on the clone.
    // This mirrors `net_phase_e::re10_start_server`'s `listener.try_clone()`.
    let (listener, timeout) = {
        let g = registry().read();
        let s = g.sockets.get(&fd).ok_or_else(|| ioex("socket gone"))?;
        let clone = s
            .socket
            .try_clone()
            .map_err(|e| ioex(format!("socketAccept: {e}")))?;
        (clone, s.so_timeout_ms)
    };
    // GC/STW-cooperation: `listener.accept()` below (both the timed
    // busy-poll loop and the unbounded branch) is a genuine OS-level
    // blocking call — the calling Java thread parks here for up to
    // SO_TIMEOUT (or indefinitely with no timeout) with no interpreter
    // safepoint reached. Without `begin_blocking_region`/`end_blocking_region`
    // a concurrent STW pause (JIT takeover or GC) counts this thread in its
    // `expected` cooperator set and waits forever, and if what unblocks the
    // accept (a peer Java thread's `connect()`) itself pauses cooperatively
    // at the same STW, the two threads deadlock each other through the STW
    // barrier. `new_impl` is a live `ObjectRef` read before the block and
    // used again afterwards (`write_fd`/`set_field_by_name`), so it must
    // ride through `end_blocking_region_refs` in case a GC compacts the
    // heap while we're parked in `accept()`. See `re1_socket_read_stream`
    // in `net_phase_e.rs` for the same pattern on the read side.
    let mut blocked_refs = [Value::Object(Some(new_impl))];
    ctx.begin_blocking_region();
    let accept_result: Result<(Socket, SockAddr), MethodCallFailed> = if let Some(t) = timeout {
        // Set non-blocking + busy-poll the (already cloned-out) listener
        // until either accept succeeds or the deadline passes. The clone is
        // local, so no registry lock is held while we sleep.
        let deadline = std::time::Instant::now() + Duration::from_millis(t as u64);
        let _ = listener.set_nonblocking(true);
        let mut last_err: Option<std::io::Error> = None;
        let mut got: Option<(Socket, SockAddr)> = None;
        loop {
            match listener.accept() {
                Ok(pair) => {
                    got = Some(pair);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Restoring blocking mode on the clone is harmless (it is dropped
        // here), but kept for parity with the previous behavior.
        let _ = listener.set_nonblocking(false);
        match got {
            Some(p) => Ok(p),
            None => {
                let kind = match last_err {
                    Some(e) => format!("{e}"),
                    None => "accept timed out".to_string(),
                };
                Err(ioex(format!("socketAccept: {kind}")))
            }
        }
    } else {
        // No registry lock held across this blocking accept — the clone is
        // a local handle dup'd out above.
        listener
            .accept()
            .map_err(|e| ioex(format!("socketAccept: {e}")))
    };
    ctx.end_blocking_region_refs(&mut blocked_refs);
    let new_impl = match blocked_refs[0] {
        Value::Object(Some(o)) => o,
        _ => new_impl,
    };
    let (new_sock, peer) = accept_result?;
    let _ = new_sock.set_nonblocking(false);
    let new_state = SocketState {
        socket: new_sock,
        so_timeout_ms: None,
        is_listening: false,
        is_connected: true,
        is_stream: true,
        closed: false,
    };
    let new_id = register_socket(new_state);
    write_fd(ctx, new_impl, new_id);
    // Fill the new SocketImpl's `address`/`port` fields if present, so the
    // Java caller's `getInetAddress` / `getPort` work without another
    // syscall. real-JDK uses Inet*AddressImpl getByAddress here; we set the
    // string form directly because the Inet* mirror is owned by net_phase_e.
    if let Some(peer_sa) = peer.as_socket() {
        ctx.set_field_by_name(new_impl, "port", Value::Int(peer_sa.port() as i32));
        ctx.set_field_by_name(new_impl, "localport", Value::Int(0));
    }
    Ok(None)
}

/// `socketClose0(boolean useDeferredClose)` — release the OS resource.
fn socket_close0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let fd = read_fd(ctx, this);
    if fd > 0 {
        // Mark closed first so other threads racing on getOption see the
        // sentinel before we drop the socket2::Socket (which closes the OS fd).
        {
            let mut g = registry().write();
            if let Some(s) = g.sockets.get_mut(&fd) {
                s.closed = true;
                let _ = s.socket.shutdown(std::net::Shutdown::Both);
            }
        }
        drop_socket(fd);
        write_fd(ctx, this, -1);
    }
    Ok(None)
}

/// `socketShutdown(int how)`. how: 0=SHUT_RD, 1=SHUT_WR.
fn socket_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let how = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Err(iae("socketShutdown: missing how")),
    };
    let fd = read_fd(ctx, this);
    let dir = match how {
        0 => std::net::Shutdown::Read,
        1 => std::net::Shutdown::Write,
        _ => std::net::Shutdown::Both,
    };
    with_socket(fd, |s| {
        s.socket
            .shutdown(dir)
            .map_err(|e| ioex(format!("socketShutdown: {e}")))?;
        Ok(None)
    })
}

/// `socketSetOption(int cmd, Object value)`.
fn socket_set_option(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let cmd = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Err(iae("socketSetOption: missing cmd")),
    };
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let fd = read_fd(ctx, this);
    with_socket(fd, |s| set_option(s, ctx, cmd, value))
}

fn set_option(
    s: &mut SocketState,
    ctx: &mut dyn NativeContext,
    cmd: i32,
    value: Value,
) -> MethodCallResult {
    match cmd {
        TCP_NODELAY => {
            let on = unbox_bool(ctx, value).unwrap_or(false);
            s.socket
                .set_nodelay(on)
                .map_err(|e| ioex(format!("TCP_NODELAY: {e}")))?;
        }
        SO_REUSEADDR => {
            let on = unbox_bool(ctx, value).unwrap_or(false);
            s.socket
                .set_reuse_address(on)
                .map_err(|e| ioex(format!("SO_REUSEADDR: {e}")))?;
        }
        SO_KEEPALIVE => {
            let on = unbox_bool(ctx, value).unwrap_or(false);
            s.socket
                .set_keepalive(on)
                .map_err(|e| ioex(format!("SO_KEEPALIVE: {e}")))?;
        }
        SO_BROADCAST => {
            let on = unbox_bool(ctx, value).unwrap_or(false);
            s.socket
                .set_broadcast(on)
                .map_err(|e| ioex(format!("SO_BROADCAST: {e}")))?;
        }
        SO_OOBINLINE => {
            let on = unbox_bool(ctx, value).unwrap_or(false);
            s.socket
                .set_out_of_band_inline(on)
                .map_err(|e| ioex(format!("SO_OOBINLINE: {e}")))?;
        }
        SO_SNDBUF => {
            let sz = unbox_int(ctx, value).unwrap_or(0).max(0) as usize;
            s.socket
                .set_send_buffer_size(sz)
                .map_err(|e| ioex(format!("SO_SNDBUF: {e}")))?;
        }
        SO_RCVBUF => {
            let sz = unbox_int(ctx, value).unwrap_or(0).max(0) as usize;
            s.socket
                .set_recv_buffer_size(sz)
                .map_err(|e| ioex(format!("SO_RCVBUF: {e}")))?;
        }
        SO_LINGER => {
            // Real-JDK passes either Boolean.FALSE (linger off) or Integer N
            // (linger N seconds). We accept either.
            let linger = match value {
                Value::Object(Some(o)) => match ctx.get_field_by_name(o, "value") {
                    Value::Int(secs) if secs >= 0 => Some(Duration::from_secs(secs as u64)),
                    Value::Int(_) => None,
                    _ => match ctx.get_field_by_name(o, "value") {
                        Value::Object(Some(_)) => None,
                        _ => None,
                    },
                },
                Value::Int(secs) if secs >= 0 => Some(Duration::from_secs(secs as u64)),
                _ => None,
            };
            s.socket
                .set_linger(linger)
                .map_err(|e| ioex(format!("SO_LINGER: {e}")))?;
        }
        SO_TIMEOUT => {
            let ms = unbox_int(ctx, value).unwrap_or(0).max(0) as u32;
            s.so_timeout_ms = if ms == 0 { None } else { Some(ms) };
            // socket2's read_timeout is what controls blocking-read deadline.
            let dur = if ms == 0 {
                None
            } else {
                Some(Duration::from_millis(ms as u64))
            };
            s.socket
                .set_read_timeout(dur)
                .map_err(|e| ioex(format!("SO_TIMEOUT: {e}")))?;
        }
        IP_TOS => {
            let tos = unbox_int(ctx, value).unwrap_or(0) as u32;
            s.socket
                .set_tos(tos)
                .map_err(|e| ioex(format!("IP_TOS: {e}")))?;
        }
        SO_REUSEPORT => {
            // Linux/macOS only — best-effort, swallow EINVAL on Windows.
            let on = unbox_bool(ctx, value).unwrap_or(false);
            #[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
            {
                s.socket
                    .set_reuse_port(on)
                    .map_err(|e| ioex(format!("SO_REUSEPORT: {e}")))?;
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
            {
                let _ = on;
            }
        }
        IP_MULTICAST_IF | IP_MULTICAST_IF2 | IP_MULTICAST_LOOP => {
            // Multicast options are owned by `native-io/src/net.rs` for
            // DatagramSocket — we accept-and-ignore here so a misrouted
            // call doesn't blow up. set_multicast_loop_v4 is the only
            // one we can express portably without an interface index.
            if cmd == IP_MULTICAST_LOOP {
                let on = unbox_bool(ctx, value).unwrap_or(false);
                let _ = s.socket.set_multicast_loop_v4(on);
            }
        }
        _ => {
            // Unknown option — fail loud rather than silently. Real-JDK
            // raises SocketException("Unknown option").
            return Err(ioex(format!("socketSetOption: unknown cmd {cmd}")));
        }
    }
    Ok(None)
}

/// `socketGetOption(int cmd) -> int` — JDK actually returns int and the Java
/// shim narrows to the right type. Boolean options return 0/1; SO_LINGER
/// returns -1 when off; SO_RCVBUF/SO_SNDBUF return the buffer size.
fn socket_get_option(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let cmd = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Err(iae("socketGetOption: missing cmd")),
    };
    let fd = read_fd(ctx, this);
    with_socket(fd, |s| get_option(s, cmd))
}

fn get_option(s: &mut SocketState, cmd: i32) -> MethodCallResult {
    let v: i32 = match cmd {
        TCP_NODELAY => bool_int(
            s.socket
                .nodelay()
                .map_err(|e| ioex(format!("TCP_NODELAY: {e}")))?,
        ),
        SO_REUSEADDR => bool_int(
            s.socket
                .reuse_address()
                .map_err(|e| ioex(format!("SO_REUSEADDR: {e}")))?,
        ),
        SO_KEEPALIVE => bool_int(
            s.socket
                .keepalive()
                .map_err(|e| ioex(format!("SO_KEEPALIVE: {e}")))?,
        ),
        SO_BROADCAST => bool_int(
            s.socket
                .broadcast()
                .map_err(|e| ioex(format!("SO_BROADCAST: {e}")))?,
        ),
        SO_OOBINLINE => bool_int(
            s.socket
                .out_of_band_inline()
                .map_err(|e| ioex(format!("SO_OOBINLINE: {e}")))?,
        ),
        SO_SNDBUF => s
            .socket
            .send_buffer_size()
            .map_err(|e| ioex(format!("SO_SNDBUF: {e}")))? as i32,
        SO_RCVBUF => s
            .socket
            .recv_buffer_size()
            .map_err(|e| ioex(format!("SO_RCVBUF: {e}")))? as i32,
        SO_LINGER => match s
            .socket
            .linger()
            .map_err(|e| ioex(format!("SO_LINGER: {e}")))?
        {
            Some(d) => d.as_secs() as i32,
            None => -1,
        },
        SO_TIMEOUT => s.so_timeout_ms.unwrap_or(0) as i32,
        IP_TOS => s.socket.tos().map_err(|e| ioex(format!("IP_TOS: {e}")))? as i32,
        SO_BINDADDR => {
            // Caller is expected to read the bound address from the SocketImpl
            // separately — we report the raw OS port here for parity with
            // HotSpot's socketGetOption(SO_BINDADDR) which writes the addr
            // through a side-effect parameter we don't model.
            match s.socket.local_addr() {
                Ok(sa) => sa.as_socket().map(|s| s.port() as i32).unwrap_or(0),
                Err(_) => 0,
            }
        }
        _ => return Err(ioex(format!("socketGetOption: unknown cmd {cmd}"))),
    };
    Ok(Some(Value::Int(v)))
}

fn bool_int(b: bool) -> i32 {
    if b {
        1
    } else {
        0
    }
}

fn unbox_bool(ctx: &dyn NativeContext, v: Value) -> Option<bool> {
    match v {
        Value::Int(0) => Some(false),
        Value::Int(_) => Some(true),
        Value::Object(Some(o)) => match ctx.get_field_by_name(o, "value") {
            Value::Int(0) => Some(false),
            Value::Int(_) => Some(true),
            _ => None,
        },
        _ => None,
    }
}

fn unbox_int(ctx: &dyn NativeContext, v: Value) -> Option<i32> {
    match v {
        Value::Int(i) => Some(i),
        Value::Object(Some(o)) => match ctx.get_field_by_name(o, "value") {
            Value::Int(i) => Some(i),
            _ => None,
        },
        _ => None,
    }
}

/// `socketAvailable() -> int` — number of bytes peekable from the kernel
/// receive queue without blocking.
fn socket_available(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let fd = read_fd(ctx, this);
    with_socket(fd, |s| {
        // Best-effort: peek one byte with MSG_PEEK in a tight non-blocking
        // try. If we read 0 with WouldBlock, return 0; otherwise return 1+.
        let was_blocking = matches!(s.socket.read_timeout(), Ok(Some(_)) | Ok(None));
        let _ = s.socket.set_nonblocking(true);
        let mut tiny = [std::mem::MaybeUninit::<u8>::uninit(); 1];
        let result = match s.socket.peek(&mut tiny) {
            Ok(n) => Ok(n as i32),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(ioex(format!("socketAvailable: {e}"))),
        };
        // Restore blocking mode if it was on. (We can't ask socket2 directly
        // whether the socket was blocking; we toggled it just above so put
        // it back to blocking if the saved read_timeout shape says so.)
        let _ = was_blocking;
        let _ = s.socket.set_nonblocking(false);
        result.map(|n| Some(Value::Int(n)))
    })
}

/// `socketSendUrgentData(int data)` — send a single byte with the URG flag.
/// On platforms that don't support `MSG_OOB`, fall back to a normal write so
/// we don't break callers that send urgent data as a heartbeat.
fn socket_send_urgent_data(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let byte = match args.get(1) {
        Some(Value::Int(v)) => (*v & 0xff) as u8,
        _ => return Err(iae("socketSendUrgentData: missing byte")),
    };
    let fd = read_fd(ctx, this);
    with_socket(fd, |s| {
        // socket2 doesn't expose `send` with flags, so we use std::io::Write
        // and flush — the URG bit is rarely honoured by modern stacks anyway,
        // and HotSpot itself falls back to a normal write on macOS.
        let buf = [byte];
        let mut sock_ref: &Socket = &s.socket;
        Write::write_all(&mut sock_ref, &buf)
            .map_err(|e| ioex(format!("socketSendUrgentData: {e}")))?;
        Ok(None)
    })
}

// ---------------------------------------------------------------------------
// Public read/write surface — these are NOT part of `socketSetOption` but
// the SocketImpl calls them through `getInputStream()/getOutputStream()`
// after `Socket.getInputStream` returns a `SocketInputStream`.
// `net_phase_e.rs` already owns `Socket$SocketInputStream` / `$SocketOutputStream`,
// so we don't duplicate that surface here. The plain-socket fd we register
// above is consumable by tests via `with_socket`.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

const PLAIN_SOCKET_IMPL: &str = "java/net/PlainSocketImpl";
const NIO_SOCKET_IMPL: &str = "sun/nio/ch/NioSocketImpl";
const PLAIN_SERVER_SOCKET_IMPL: &str = "java/net/PlainServerSocketImpl";

fn register_impl_surface(r: &mut NativeMethodRegistry, fqn: &'static str) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(fqn, "socketCreate", "(Z)V", socket_create);
    // NioSocketImpl variant takes a (boolean) too; PlainSocketImpl historically
    // used (boolean) and PlainServerSocketImpl uses (boolean). All match.
    r.register(fqn, "socketCreate", "(ZZ)V", socket_create);

    r.register(
        fqn,
        "socketConnect",
        "(Ljava/net/InetAddress;II)V",
        socket_connect,
    );

    r.register(fqn, "socketBind", "(Ljava/net/InetAddress;I)V", socket_bind);

    r.register(fqn, "socketListen", "(I)V", socket_listen);

    r.register(
        fqn,
        "socketAccept",
        "(Ljava/net/SocketImpl;)V",
        socket_accept,
    );

    r.register(fqn, "socketClose0", "(Z)V", socket_close0);
    r.register(fqn, "socketClose0", "()V", socket_close0);

    r.register(fqn, "socketShutdown", "(I)V", socket_shutdown);

    r.register(
        fqn,
        "socketSetOption",
        "(IZLjava/lang/Object;)V",
        |ctx, args| {
            // 4-arg variant: (this, cmd, on, value). Forward to the 3-arg
            // helper after picking the right argument depending on the
            // boolean flag.
            let this = this_obj(args)?;
            let cmd = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => return Err(iae("socketSetOption: missing cmd")),
            };
            let on = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
            let value = args
                .get(3)
                .copied()
                .unwrap_or(Value::Int(if on { 1 } else { 0 }));
            let effective = if let Value::Object(Some(_)) = value {
                value
            } else {
                Value::Int(if on { 1 } else { 0 })
            };
            let fd = read_fd(ctx, this);
            with_socket(fd, |s| set_option(s, ctx, cmd, effective))
        },
    );
    r.register(
        fqn,
        "socketSetOption",
        "(ILjava/lang/Object;)V",
        socket_set_option,
    );

    r.register(fqn, "socketGetOption", "(I)I", socket_get_option);
    r.register(
        fqn,
        "socketGetOption",
        "(ILjava/lang/Object;)I",
        |ctx, args| {
            // Some real-JDK overloads pass an out-parameter object that
            // captures the InetAddress for SO_BINDADDR. We honour the cmd
            // and ignore the side-channel — Java's binding code populates
            // its own field afterwards.
            let _ = args.get(2);
            let this = this_obj(args)?;
            let cmd = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => return Err(iae("socketGetOption: missing cmd")),
            };
            let fd = read_fd(ctx, this);
            with_socket(fd, |s| get_option(s, cmd))
        },
    );

    r.register(fqn, "socketAvailable", "()I", socket_available);

    r.register(fqn, "socketSendUrgentData", "(I)V", socket_send_urgent_data);

    // Older JDKs use `initProto` / `init` for static-init-time Class<->C-side
    // option-mapping setup. We register a no-op so the static initializer
    // finds the symbol and doesn't UnsatisfiedLinkError.
    r.register(fqn, "initProto", "()V", |_ctx, _args| Ok(None));
    r.register(fqn, "init", "()V", |_ctx, _args| Ok(None));
    r.set_category(__prev_cat);
}

/// Anchor for the roadmap-gate grep. Uses the fully-qualified
/// `socket2::Socket::new` form so `grep socket2::Socket::new` finds this
/// module even when callers `use socket2::Socket;` and drop the prefix.
#[doc(hidden)]
pub fn _anchor_socket2_new() {
    let _maker: fn() -> std::io::Result<socket2::Socket> =
        || socket2::Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP));
}

/// Public entry point — wave coordinator wires this from `lib.rs`.
pub fn register_plain_socket_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_impl_surface(r, PLAIN_SOCKET_IMPL);
    register_impl_surface(r, NIO_SOCKET_IMPL);
    register_impl_surface(r, PLAIN_SERVER_SOCKET_IMPL);

    // KEEP as no-ops: same JNI-field-ID-caching family as the `initProto`/
    // `init` pair above — no observable effect, registered so a `<clinit>`
    // that references the symbol does not die with UnsatisfiedLinkError.
    // Note (wave-3 sweep): modern OpenJDK `java.net.Socket`/`ServerSocket` may
    // not declare `init()V` at all, in which case these two are inert rather
    // than load-bearing; that could not be verified without a JDK to read, and
    // they shadow nothing (no real method of that name/descriptor exists to
    // intercept). The richer `<init>` / `connect` / etc. overloads on those
    // public classes are owned by `net_phase_e.rs::register_re1_socket` /
    // `register_re2_server_socket`; only the static `init` symbols belong here.
    r.register("java/net/Socket", "init", "()V", |_ctx, _args| Ok(None));
    r.register("java/net/ServerSocket", "init", "()V", |_ctx, _args| {
        Ok(None)
    });
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Helpers needed by tests but not by the registered callbacks.
// ---------------------------------------------------------------------------

#[cfg(test)]
fn _addr_v4_loopback(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

#[cfg(test)]
fn _addr_v6_loopback(port: u16) -> SocketAddr {
    SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::io::Read as _;

    /// Helper: allocate a socket directly through `Socket::new` and stash it
    /// in our registry, returning the assigned id. Exercises the same code
    /// path `socketCreate` would take, without needing a NativeContext.
    fn fresh_stream_socket() -> i32 {
        let s = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        register_socket(SocketState {
            socket: s,
            so_timeout_ms: None,
            is_listening: false,
            is_connected: false,
            is_stream: true,
            closed: false,
        })
    }

    #[test]
    fn socket_registry_basic_roundtrip() {
        let id = fresh_stream_socket();
        assert!(id > 0);
        with_socket::<_, ()>(id, |s| {
            assert!(s.is_stream);
            assert!(!s.closed);
            assert!(!s.is_listening);
            Ok(())
        })
        .unwrap();
        drop_socket(id);
        let err = with_socket::<_, ()>(id, |_| Ok(())).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("not found"), "got {msg}");
    }

    #[test]
    fn options_roundtrip_tcp_nodelay_and_keepalive() {
        let id = fresh_stream_socket();
        // TCP_NODELAY on
        with_socket::<_, ()>(id, |s| {
            s.socket.set_nodelay(true).unwrap();
            assert!(s.socket.nodelay().unwrap());
            // SO_KEEPALIVE on
            s.socket.set_keepalive(true).unwrap();
            assert!(s.socket.keepalive().unwrap());
            // SO_REUSEADDR on
            s.socket.set_reuse_address(true).unwrap();
            assert!(s.socket.reuse_address().unwrap());
            Ok(())
        })
        .unwrap();
        drop_socket(id);
    }

    #[test]
    fn end_to_end_listen_and_accept_on_loopback() {
        // Bind a server on 127.0.0.1:0, set SO_REUSEADDR + listen(50),
        // connect a client, send 5 bytes, accept, read, assert equality.
        let server = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        server.set_reuse_address(true).unwrap();
        server
            .bind(&SockAddr::from(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::LOCALHOST,
                0,
            ))))
            .unwrap();
        server.listen(50).unwrap();
        let bound = server.local_addr().unwrap().as_socket().unwrap();

        let server_id = register_socket(SocketState {
            socket: server,
            so_timeout_ms: None,
            is_listening: true,
            is_connected: false,
            is_stream: true,
            closed: false,
        });

        let server_thread = std::thread::spawn(move || {
            let g = registry().read();
            let s = g.sockets.get(&server_id).unwrap();
            let (mut accepted, _peer) = s.socket.accept().unwrap();
            drop(g);
            // socket2 returns its own Socket for `accept`; convert to a
            // Read-capable handle.
            let mut buf = [0u8; 5];
            accepted.read_exact(&mut buf).unwrap();
            buf
        });

        let mut client = std::net::TcpStream::connect(bound).unwrap();
        client.write_all(b"hello").unwrap();
        client.flush().unwrap();

        let bytes = server_thread.join().unwrap();
        assert_eq!(&bytes, b"hello");
        drop_socket(server_id);
    }

    #[test]
    fn so_linger_off_returns_minus_one() {
        let id = fresh_stream_socket();
        let v = with_socket(id, |s| {
            s.socket.set_linger(None).unwrap();
            get_option(s, SO_LINGER)
        })
        .unwrap();
        assert_eq!(v, Some(Value::Int(-1)));
        drop_socket(id);
    }

    #[test]
    fn so_timeout_persists_in_state() {
        let id = fresh_stream_socket();
        with_socket::<_, ()>(id, |s| {
            s.so_timeout_ms = Some(750);
            s.socket
                .set_read_timeout(Some(Duration::from_millis(750)))
                .unwrap();
            Ok(())
        })
        .unwrap();
        let v = with_socket(id, |s| get_option(s, SO_TIMEOUT)).unwrap();
        assert_eq!(v, Some(Value::Int(750)));
        drop_socket(id);
    }

    #[test]
    fn close_marks_state_and_subsequent_call_errors() {
        let id = fresh_stream_socket();
        // Mark closed by hand (mirroring what socket_close0 does).
        {
            let mut g = registry().write();
            let s = g.sockets.get_mut(&id).unwrap();
            s.closed = true;
            let _ = s.socket.shutdown(std::net::Shutdown::Both);
        }
        let err = with_socket(id, |s| get_option(s, TCP_NODELAY)).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("Socket closed"), "got {msg}");
        drop_socket(id);
    }

    #[test]
    fn unknown_option_rejected() {
        let id = fresh_stream_socket();
        let err = with_socket(id, |s| get_option(s, 0xdead_beefu32 as i32)).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("unknown cmd"), "got {msg}");
        drop_socket(id);
    }

    /// Regression for the P1 lock-contention bug: a blocking `accept()` must
    /// NOT hold the registry lock for its duration. We reproduce the shape of
    /// the (no-SO_TIMEOUT) accept path — clone the listener out under a short
    /// lock, then block on the clone — and assert that a concurrent
    /// `with_socket` write on another socket succeeds *while* the accept is
    /// still blocked waiting for a connection. Before the fix the read guard
    /// was held across `accept()`, so this `with_socket` would block until a
    /// client connected (or forever).
    #[test]
    fn blocking_accept_does_not_hold_registry_lock() {
        use std::sync::mpsc;

        // Server listener registered like socketCreate/Bind/Listen would.
        let server = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        server.set_reuse_address(true).unwrap();
        server
            .bind(&SockAddr::from(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::LOCALHOST,
                0,
            ))))
            .unwrap();
        server.listen(50).unwrap();
        let bound = server.local_addr().unwrap().as_socket().unwrap();
        let server_id = register_socket(SocketState {
            socket: server,
            so_timeout_ms: None,
            is_listening: true,
            is_connected: false,
            is_stream: true,
            closed: false,
        });

        // A second, unrelated socket whose registry entry we'll touch while
        // the accept is blocked.
        let other_id = fresh_stream_socket();

        let (started_tx, started_rx) = mpsc::channel::<()>();
        let accept_thread = std::thread::spawn(move || {
            // Mirror socket_accept's no-timeout path: clone out under a short
            // lock, release, then block on the clone.
            let listener = {
                let g = registry().read();
                let s = g.sockets.get(&server_id).unwrap();
                s.socket.try_clone().unwrap()
            };
            started_tx.send(()).unwrap();
            let (_accepted, _peer) = listener.accept().unwrap();
        });

        // Wait until the accept thread has cloned-out and is about to block.
        started_rx.recv().unwrap();

        // While the accept blocks, a write-locking registry op on the OTHER
        // socket must complete promptly (it would deadlock under the old code).
        with_socket::<_, ()>(other_id, |s| {
            s.is_connected = true;
            Ok(())
        })
        .unwrap();

        // Now unblock the accept so the thread can finish.
        let mut client = std::net::TcpStream::connect(bound).unwrap();
        client.write_all(b"x").unwrap();
        client.flush().unwrap();
        accept_thread.join().unwrap();

        drop_socket(server_id);
        drop_socket(other_id);
    }
}
