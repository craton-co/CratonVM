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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

fn ipc_dbg_enabled() -> bool {
    std::env::var("CRATONVM_SUREFIRE_IPC_DBG")
        .map(|v| {
            let t = v.trim();
            !t.is_empty() && t != "0" && !t.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
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
    Stream(TcpStream),
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
        // A connect-in-progress socket is a live pollable fd: clone it as a
        // Stream so the selector polls it for write-readiness and surfaces
        // OP_CONNECT naturally once the OS completes (or refuses) the connect.
        Some(TcpHandle::Connecting(s)) => s.try_clone().ok().map(TcpHandleClone::Stream),
        _ => None,
    }
}

pub(crate) enum TcpHandleClone {
    Listener(TcpListener),
    Stream(TcpStream),
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
    let map = tcp_registry().read();
    match map.get(&net_fd) {
        Some(TcpHandle::Connecting(s)) => match crate::nb_connect::poll(s) {
            crate::nb_connect::ConnectPoll::Pending => SelectorConnectProbe::Pending,
            // Both success and failure must surface OP_CONNECT so the reactor
            // calls finishConnect(), which reads SO_ERROR and reports the
            // outcome (a ConnectException on failure) rather than hanging.
            crate::nb_connect::ConnectPoll::Connected
            | crate::nb_connect::ConnectPoll::Failed(_) => SelectorConnectProbe::Ready,
        },
        Some(TcpHandle::Stream(_)) => SelectorConnectProbe::Ready,
        _ => SelectorConnectProbe::NotConnecting,
    }
}

/// Per-fd non-blocking flag. The OS state on the real socket mirrors this.
fn tcp_blocking_state() -> &'static RwLock<HashMap<i32, bool>> {
    static FLAGS: OnceLock<RwLock<HashMap<i32, bool>>> = OnceLock::new();
    FLAGS.get_or_init(|| RwLock::new(HashMap::new()))
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
}

const ACCEPT_CLOSE_POLL: Duration = Duration::from_millis(10);
const LINGERING_CHANNEL_CLOSE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

fn tcp_listener_still_registered(id: i32) -> bool {
    matches!(tcp_registry().read().get(&id), Some(TcpHandle::Listener(_)))
}

fn accept_close_aware(
    listener: &TcpListener,
    id: i32,
    blocking: bool,
) -> std::io::Result<Option<(TcpStream, SocketAddr)>> {
    listener.set_nonblocking(true)?;

    if !blocking {
        return match listener.accept() {
            Ok(pair) => Ok(Some(pair)),
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        };
    }

    loop {
        match listener.accept() {
            Ok(pair) => return Ok(Some(pair)),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if !tcp_listener_still_registered(id) {
                    return Err(std::io::Error::new(
                        ErrorKind::Interrupted,
                        "server channel closed",
                    ));
                }
                std::thread::sleep(ACCEPT_CLOSE_POLL);
            }
            Err(e) => return Err(e),
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
            message: format!("{ctx}: {e}"),
        }
        .into(),
        ErrorKind::TimedOut => RuntimeError::SocketTimeoutException {
            message: format!("{ctx}: {e}"),
        }
        .into(),
        ErrorKind::AddrInUse => ioex(format!("BindException: Address already in use: {ctx}: {e}")),
        ErrorKind::AddrNotAvailable => {
            ioex(format!("BindException: Cannot assign requested address: {ctx}: {e}"))
        }
        ErrorKind::PermissionDenied => {
            ioex(format!("BindException: Permission denied: {ctx}: {e}"))
        }
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset => {
            ioex(format!("SocketException: {ctx}: {e}"))
        }
        ErrorKind::NotConnected => ioex(format!("SocketException: Not connected: {ctx}: {e}")),
        _ => ioex(format!("SocketException: {ctx}: {e}")),
    }
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
fn init_channel_locks(ctx: &mut dyn NativeContext, ch: ObjectRef) {
    // Seed each monitor field with the channel object ITSELF rather than a fresh
    // `new Object()`. The fields only need to be a non-null, stable monitor; the
    // channel is one, and using it avoids the allocation entirely — which matters
    // because `new_object` can trigger a moving GC that relocates `ch`, and under
    // concurrent load (the Apache reactor opening hundreds of channels) that race
    // left `closeLock` null for a few channels → reactor-killing NPE on close (ES
    // testManyAsyncRequests). `synchronized(closeLock)` then `synchronized(keyLock)`
    // both lock the same channel monitor reentrantly (same thread) — correct, and
    // distinct channels still use distinct monitors.
    for f in ["closeLock", "keyLock", "regLock"] {
        if !matches!(ctx.get_field_by_name(ch, f), Value::Object(Some(_))) {
            ctx.set_field_by_name(ch, f, Value::Object(Some(ch)));
        }
    }
}

/// Layout convention:
///   field 0 = open (1=open, 0=closed)
///   field 1 = blocking (1=blocking, 0=non-blocking)
///   field 2 = registry id (i32 into `tcp_registry`)
///   field 3 = connected (1=connected, 0=not)
///   field 4 = local port (i32, 0=unset)
///   field 5 = remote address text (String or null)
///   field 6 = remote port (i32, 0=unset)
const F_OPEN: usize = 0;
const F_BLOCKING: usize = 1;
const F_REG_ID: usize = 2;
const F_CONNECTED: usize = 3;
const F_LOCAL_PORT: usize = 4;
const F_REMOTE: usize = 5;
const F_REMOTE_PORT: usize = 6;
const N_FIELDS: usize = 7;

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

fn chan_fields() -> &'static RwLock<HashMap<i32, [Syn; N_FIELDS]>> {
    static T: OnceLock<RwLock<HashMap<i32, [Syn; N_FIELDS]>>> = OnceLock::new();
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
    let arr = t.entry(key).or_insert_with(default_syn);
    arr[idx] = slot;
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
    match chan_fields().read().get(&key).map(|a| &a[idx]) {
        Some(Syn::I(i)) => Value::Int(*i),
        _ => Value::Int(0),
    }
}

/// Drop a channel object's synthetic state (called on close) so the table
/// does not grow without bound across short-lived connections.
fn cf_clear(ctx: &dyn NativeContext, obj: ObjectRef) {
    let key = ctx.identity_hash_code(obj);
    chan_fields().write().remove(&key);
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
    let arr = t.get(&key)?;
    let host = match &arr[F_REMOTE] {
        Syn::S(s) => s.clone(),
        _ => return None,
    };
    let port = match arr[F_REMOTE_PORT] {
        Syn::I(p) => p,
        _ => 0,
    };
    Some((host, port))
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
    let ch = alloc_obj(ctx, "java/nio/channels/SocketChannel", N_FIELDS);
    init_channel_locks(ctx, ch);
    cf_set(ctx, ch, F_OPEN, Value::Int(1));
    cf_set(ctx, ch, F_BLOCKING, Value::Int(1));
    cf_set(ctx, ch, F_REG_ID, Value::Int(-1));
    cf_set(ctx, ch, F_CONNECTED, Value::Int(0));
    cf_set(ctx, ch, F_LOCAL_PORT, Value::Int(0));
    cf_set(ctx, ch, F_REMOTE, Value::Object(None));
    cf_set(ctx, ch, F_REMOTE_PORT, Value::Int(0));
    Ok(Some(Value::Object(Some(ch))))
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
            let bound = cf_get(ctx, o, F_LOCAL_PORT).as_int().unwrap_or(0) > 0;
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
    let pending = matches!(
        tcp_registry().read().get(&id),
        Some(TcpHandle::Connecting(_))
    );
    Ok(Some(Value::Int(if pending { 1 } else { 0 })))
}

/// `SocketChannelImpl.isInputOpen()` / `isOutputOpen()` (package-private) —
/// consulted by sun.nio.ch.SocketAdaptor's input/output streams (the streams
/// returned by socket().getInputStream()/getOutputStream()). CratonVM does not
/// track half-close separately, so report open whenever the channel is open and
/// connected. Without these, SocketAdaptor.getOutputStream NoSuchMethodErrors.
fn sc_io_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) => {
            let open = matches!(cf_get(ctx, o, F_OPEN), Value::Int(1));
            Ok(Some(Value::Int(if open { 1 } else { 0 })))
        }
        _ => Ok(Some(Value::Int(0))),
    }
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
            // docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md. Kept as a
            // permanent opt-in hook (zero cost when unset) for whoever
            // continues that investigation, matching CRATONVM_DBG_NET /
            // CRATONVM_DBG_STALE_RECV etc.
            if std::env::var_os("CRATONVM_DBG_SC_CLOSE").is_some() {
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
                eprintln!(
                    "[SC_CLOSE] t={ms} id={id:#x} local={local} peer={peer}"
                );
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
                eprintln!("[SC_CLOSE_STACK] t={ms} id={id:#x} ({} frames)", raw_trace.len());
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

/// No-op override for `java.net.Socket` option setters. The `Socket` returned
/// by `SocketChannel.socket()` is a bare adapter with no real `SocketImpl`, so
/// the real setter bytecode would call `getImpl()` and NPE. These options are
/// best-effort (the channel carries the live socket), so swallow them. When
/// `CRATONVM_REAL_NET_SOCKETS` is set the central registry filter drops every
/// `java/net/Socket` registration, so the real java.net path is used instead.
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
    let id = match read_reg_id(ctx, this) {
        Some(i) => i,
        None => return Ok(Some(Value::Object(None))),
    };
    let local = {
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Stream(s)) => s.local_addr().ok(),
            _ => None,
        }
    };
    let Some(addr) = local else {
        return Ok(Some(Value::Object(None)));
    };
    new_resolved_inet_socket_address(ctx, &addr.ip().to_string(), addr.port() as i32)
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

    let addrs: Vec<SocketAddr> = match target.to_socket_addrs() {
        Ok(it) => it.collect(),
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
fn sc_connect_inner(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    sa: ObjectRef,
    allow_block: bool,
) -> Result<bool, MethodCallFailed> {
    let (host, port) = decode_socket_address(ctx, sa)?;
    let target = format!("{host}:{port}");
    ipc_dbg(format!("connect target={target} allow_block={allow_block}"));

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
        let id = tcp_register(TcpHandle::Stream(stream));
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
    let mut last_err: Option<std::io::Error> = None;
    for addr in &vetted {
        match crate::nb_connect::start(addr) {
            Ok(crate::nb_connect::StartConnect::Connected(stream)) => {
                let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
                let id = tcp_register(TcpHandle::Stream(stream));
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
            Err(e) => {
                ipc_dbg(format!("connect start failed addr={addr}: {e}"));
                last_err = Some(e);
            }
        }
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
        None => return Err(ioex("connect: null SocketAddress")),
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
        None => return Err(ioex("blockingConnect: null SocketAddress")),
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
                    map.insert(id, TcpHandle::Stream(stream));
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
fn try_read_nb(stream: &TcpStream, buf: &mut [u8]) -> Result<Option<i32>, std::io::Error> {
    let mut s = stream;
    match s.read(buf) {
        Ok(0) => Ok(Some(-1)),
        Ok(n) => Ok(Some(n as i32)),
        Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => Ok(None),
        Err(e) if e.kind() == ErrorKind::Interrupted => Ok(Some(0)),
        Err(e) => Err(e),
    }
}

fn try_write_nb(stream: &TcpStream, data: &[u8]) -> Result<Option<i32>, std::io::Error> {
    let mut s = stream;
    match s.write(data) {
        Ok(n) => Ok(Some(n as i32)),
        Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
        Err(e) if e.kind() == ErrorKind::Interrupted => Ok(Some(0)),
        Err(e) => Err(e),
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
    let id = read_reg_id(ctx, this).ok_or_else(|| ioex("read: channel not connected"))?;

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
    ctx.begin_blocking_region();
    let read_result = {
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Stream(s)) => {
                let r = try_read_nb(s, &mut buf).map_err(|e| map_err("read", e));
                ctx.end_blocking_region();
                r
            }
            Some(TcpHandle::Connecting(_)) => {
                ctx.end_blocking_region();
                ctx.unpin_native_roots(bb_pin);
                return Ok(Some(Value::Int(0)));
            }
            _ => {
                ctx.end_blocking_region();
                ctx.unpin_native_roots(bb_pin);
                return Err(ioex("read: channel not a stream"));
            }
        }
    };

    let n_opt = match read_result {
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
        crate::net::socket_capture('r', id, &buf[..n as usize]);
        let bb = ctx.read_native_pin(bb_pin, bb);
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
    let write_result = {
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Stream(s)) => {
                let r = try_write_nb(s, &data).map_err(|e| map_err("write", e));
                ctx.end_blocking_region();
                r
            }
            Some(TcpHandle::Connecting(_)) => {
                ctx.end_blocking_region();
                ctx.unpin_native_roots(bb_pin);
                return Ok(Some(Value::Int(0)));
            }
            _ => {
                ctx.end_blocking_region();
                ctx.unpin_native_roots(bb_pin);
                return Err(ioex("write: channel not a stream"));
            }
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
        for (pin, _, _) in chunks {
            ctx.unpin_native_roots(pin);
        }
        return Ok(Some(Value::Long(0)));
    }
    let mut data = Vec::with_capacity(total);
    for (_, _, bytes) in &chunks {
        data.extend_from_slice(bytes);
    }

    let write_result = {
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Stream(s)) => {
                try_write_nb(s, &data).map_err(|e| map_err("write(gathering)", e))
            }
            Some(TcpHandle::Connecting(_)) => {
                for (pin, _, _) in &chunks {
                    ctx.unpin_native_roots(*pin);
                }
                return Ok(Some(Value::Long(0)));
            }
            _ => {
                for (pin, _, _) in &chunks {
                    ctx.unpin_native_roots(*pin);
                }
                return Err(ioex("write(gathering): channel not a stream"));
            }
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
    let id =
        read_reg_id(ctx, this).ok_or_else(|| ioex("read(scattering): channel not connected"))?;

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

    let n_opt = {
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Stream(s)) => {
                try_read_nb(s, &mut buf).map_err(|e| map_err("read(scattering)", e))?
            }
            Some(TcpHandle::Connecting(_)) => return Ok(Some(Value::Long(0))),
            _ => return Err(ioex("read(scattering): channel not a stream")),
        }
    };
    let n = match n_opt {
        Some(v) => v,
        None => return Ok(Some(Value::Long(0))), // EAGAIN
    };
    if n < 0 {
        return Ok(Some(Value::Long(-1))); // EOF
    }
    if n > 0 {
        crate::net::socket_capture('r', id, &buf[..n as usize]);
        // Scatter the bytes into the destination buffers in order; each call
        // fills one buffer up to its remaining room, then we move to the next.
        let mut consumed = 0usize;
        for bb in &targets {
            if consumed >= n as usize {
                break;
            }
            let written = buffer_write_bytes(ctx, *bb, &buf[consumed..n as usize]);
            if written <= 0 {
                break;
            }
            buffer_advance(ctx, *bb, written);
            consumed += written as usize;
        }
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
        _ => Ok(0),
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
    let val = int_arg(args, 2);

    if let Some(id) = read_reg_id(ctx, this) {
        let map = tcp_registry().read();
        if let Some(TcpHandle::Stream(s)) = map.get(&id) {
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
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Stream(s)) => read_option(s, &opt_name).unwrap_or(0),
            _ => 0,
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
/// docs/known-issues/spring-web-flow-outputstreamwriter-close-corruption.md
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

// ---------------------------------------------------------------------------
// ServerSocketChannel — open / bind / accept / close
// ---------------------------------------------------------------------------

fn ssc_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let ch = alloc_obj(ctx, "java/nio/channels/ServerSocketChannel", N_FIELDS);
    init_channel_locks(ctx, ch);
    cf_set(ctx, ch, F_OPEN, Value::Int(1));
    cf_set(ctx, ch, F_BLOCKING, Value::Int(1));
    cf_set(ctx, ch, F_REG_ID, Value::Int(-1));
    cf_set(ctx, ch, F_CONNECTED, Value::Int(0));
    cf_set(ctx, ch, F_LOCAL_PORT, Value::Int(0));
    cf_set(ctx, ch, F_REMOTE, Value::Object(None));
    cf_set(ctx, ch, F_REMOTE_PORT, Value::Int(0));
    Ok(Some(Value::Object(Some(ch))))
}

fn ssc_bind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("bind: null channel")),
    };
    let sa = match obj_or_none(args, 1) {
        Some(o) => o,
        None => return Err(ioex("bind: null SocketAddress")),
    };
    // arg[2] is the backlog — std::net::TcpListener picks its own.
    let _backlog = int_arg(args, 2).max(0);

    let (host, port) = decode_socket_address(ctx, sa)?;
    // host==""/"0.0.0.0"/"::" maps to wildcard.
    let bind_text = if host.is_empty() {
        format!("0.0.0.0:{port}")
    } else {
        format!("{host}:{port}")
    };
    let listener = TcpListener::bind(&bind_text).map_err(|e| map_err(&bind_text, e))?;
    let local_port = listener
        .local_addr()
        .map(|a| a.port() as i32)
        .unwrap_or(port as i32);
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

fn ssc_accept(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("accept: null channel")),
    };
    let id = read_reg_id(ctx, this).ok_or_else(|| ioex("accept: server channel not bound"))?;
    let blocking = read_blocking_flag(ctx, this);

    // Wave 3 Task C: the selector loop pre-drains pending accepts when
    // OP_ACCEPT fires (see `kernel_select_*` in nio_selector.rs); pull
    // from that side-channel first so we don't block on a queue that
    // has already been emptied.
    let preaccepted = crate::nio_selector::take_any_pending_accepted(id);

    // Clone listener out so the registry lock isn't held across blocking accept.
    let listener_clone = {
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Listener(l)) => {
                l.try_clone().map_err(|e| map_err("accept clone", e))?
            }
            _ => return Err(ioex("accept: id is not a listener")),
        }
    };
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
        // nonblocking poll loop: close() drops the registry entry, which wakes
        // this path promptly even if the OS would leave our duplicate listener
        // blocked in accept().
        let res = if blocking {
            ctx.begin_blocking_region();
            let res = accept_close_aware(&listener_clone, id, true);
            ctx.end_blocking_region();
            res
        } else {
            accept_close_aware(&listener_clone, id, false)
        };
        match res {
            Ok(pair) => pair,
            Err(e) => return Err(map_err("accept", e)),
        }
    };

    let Some((stream, peer)) = accepted else {
        return Ok(Some(Value::Object(None)));
    };

    // Inherit non-blocking flag of the parent channel.
    let _ = stream.set_nonblocking(!blocking);

    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    let new_id = tcp_register(TcpHandle::Stream(stream));
    tcp_blocking_state().write().insert(new_id, blocking);

    let child = alloc_obj(ctx, "java/nio/channels/SocketChannel", N_FIELDS);
    init_channel_locks(ctx, child);
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
        r.register(
            c,
            "blockingConnect",
            "(Ljava/net/SocketAddress;J)V",
            sc_blocking_connect,
        );
        r.register(c, "finishConnect", "()Z", sc_finish_connect);
        r.register(c, "isConnectionPending", "()Z", sc_is_connection_pending);
        r.register(c, "isInputOpen", "()Z", sc_io_open);
        r.register(c, "isOutputOpen", "()Z", sc_io_open);
        r.register(c, "read", "(Ljava/nio/ByteBuffer;)I", sc_read);
        r.register(c, "write", "(Ljava/nio/ByteBuffer;)I", sc_write);
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
        r.register(
            c,
            "accept",
            "()Ljava/nio/channels/SocketChannel;",
            ssc_accept,
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
    // SSC at SS_CHANNEL_REF. The bind / getLocalPort / accept / close /
    // getInetAddress methods detect this back-ref and delegate to the
    // owning channel; without a back-ref we fall through to defaults so
    // that plain `new ServerSocket()` use cases (handled elsewhere) are
    // not perturbed.
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
    // SocketImpl so the real bytecode would NPE in getImpl(). No-op them
    // (gate-aware: dropped under CRATONVM_REAL_NET_SOCKETS).
    let client_socket = "java/net/Socket";
    for (m, d) in [
        ("setReceiveBufferSize", "(I)V"),
        ("setSendBufferSize", "(I)V"),
        ("setKeepAlive", "(Z)V"),
        ("setReuseAddress", "(Z)V"),
        ("setTcpNoDelay", "(Z)V"),
        ("setOOBInline", "(Z)V"),
        ("setSoLinger", "(ZI)V"),
        ("setSoTimeout", "(I)V"),
        ("setPerformancePreferences", "(III)V"),
    ] {
        r.register(client_socket, m, d, socket_opt_noop);
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

fn ss_back_ref_table() -> &'static RwLock<rustc_hash::FxHashMap<i32, ObjectRef>> {
    static REG: OnceLock<RwLock<rustc_hash::FxHashMap<i32, ObjectRef>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(rustc_hash::FxHashMap::default()))
}

fn ss_record_back_ref(ctx: &mut dyn NativeContext, ss: ObjectRef, ssc: ObjectRef) {
    let key = ctx.identity_hash_code(ss);
    ss_back_ref_table().write().insert(key, ssc);
}

fn ss_back_ref(ctx: &mut dyn NativeContext, ss: ObjectRef) -> Option<ObjectRef> {
    let key = ctx.identity_hash_code(ss);
    ss_back_ref_table().read().get(&key).copied()
}

/// Post-GC hook — remap the SSC `ObjectRef` values that
/// `ss_back_ref_table` stores. The KEYS are identity hash codes and are
/// already GC-stable, so they need no rewrite; only the embedded
/// ObjectRef values are repointed through `pointer_map`. Mirrors
/// `gc_update_lambda_callsite_cache_refs` in
/// `native-builtins/src/lang_invoke.rs`. Until this hook is wired into
/// `vm/src/memory/gc.rs`'s post-compaction step, the table will return
/// stale `ObjectRef` values for any SSC that was relocated. The
/// identity-hash key fix alone eliminates the use-after-free risk that
/// the previous `from_raw(usize)` resurrection carried — the worst-case
/// behaviour now is a missed lookup rather than a wild dereference.
#[allow(dead_code)]
pub fn ss_back_ref_update_after_gc(pointer_map: &rustc_hash::FxHashMap<usize, usize>) {
    if pointer_map.is_empty() {
        return;
    }
    let mut table = ss_back_ref_table().write();
    for v in table.values_mut() {
        let old = v.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            // SAFETY: `new_addr` is the GC's relocated address for the
            // same logical SSC object; the GC guarantees the new
            // address is a valid heap object that satisfies
            // ObjectRef's non-null/alignment invariants.
            *v = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
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
    if std::env::var_os("CRATONVM_REAL_NET_SOCKETS").is_some() {
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

fn ssc_local_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
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
            .map(|a| a.ip().to_string())
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
    let h = ctx.create_string(host);
    if let Ok(Some(Value::Object(Some(addr)))) = ctx.invoke(
        "java/net/InetAddress",
        "getByName",
        "(Ljava/lang/String;)Ljava/net/InetAddress;",
        &[Value::Object(Some(h))],
    ) {
        return ctx.new_object_initialized(
            "java/net/InetSocketAddress",
            "(Ljava/net/InetAddress;I)V",
            &[Value::Object(Some(addr)), Value::Int(port)],
        );
    }
    let h = ctx.create_string(host);
    ctx.new_object_initialized(
        "java/net/InetSocketAddress",
        "(Ljava/lang/String;I)V",
        &[Value::Object(Some(h)), Value::Int(port)],
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
        // reads + port recording) lives in native-builtins, which we cannot
        // call directly. Delegate through the cross-crate hook it installs
        // (BUG-04); previously this no-opped, leaving `getLocalPort()` = 0.
        if let Some(cb) = cratonvm_native_api::plain_server_socket_bind::get() {
            return cb(ctx, args);
        }
        return Ok(None);
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
        // Plain ServerSocket — delegate to the native-builtins plain-bind hook
        // (BUG-04); see ss_wrapper_bind above.
        if let Some(cb) = cratonvm_native_api::plain_server_socket_bind::get() {
            return cb(ctx, args);
        }
        return Ok(None);
    };
    let sa = args.get(1).copied().unwrap_or(Value::Object(None));
    let backlog = args.get(2).copied().unwrap_or(Value::Int(50));
    let _ = ssc_bind(ctx, &[Value::Object(Some(ssc)), sa, backlog])?;
    Ok(None)
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
    // (bound by the net_phase_e re2 / phases_early phase53 path), not a channel adapter.
    // This native is the last-registered `getLocalPort` and therefore shadows the plain
    // ServerSocket too, so returning 0 here breaks every plain-socket caller that reads
    // its bound port (e.g. Narayana's TransactionStatusManager advertises getLocalPort()
    // and its recovery connector then connects to it → the Hibernate JTA cluster hang).
    // The binding native records the actual OS-assigned port in the shared native-api
    // registry keyed by identity hash (object fields can't carry it — the real layout's
    // low slots are reference-typed, so an int does not round-trip). Read it back.
    let p =
        cratonvm_native_api::server_socket_ports::get(ctx.identity_hash_code(this)).unwrap_or(0);
    Ok(Some(Value::Int(p)))
}

fn ss_wrapper_local_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let (port, host) = if let Some(ssc) = ss_back_ref(ctx, this) {
        let port = cf_get(ctx, ssc, F_LOCAL_PORT).as_int().unwrap_or(0);
        let id = cf_get(ctx, ssc, F_REG_ID).as_int().unwrap_or(-1);
        // Real bound address, not the historical "0.0.0.0" placeholder —
        // see `ssc_local_address` (same registry, same rationale).
        let host = match tcp_registry().read().get(&id) {
            Some(TcpHandle::Listener(l)) => l
                .local_addr()
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|_| "0.0.0.0".to_string()),
            _ => "0.0.0.0".to_string(),
        };
        (port, host)
    } else {
        // Plain ServerSocket — read the address recorded by the binder (BUG-04),
        // same channel ss_wrapper_local_port uses. The binder lives in
        // native-builtins, while this last-registered wrapper lives in native-io,
        // so the native-api side table is the cross-crate handoff.
        let identity = ctx.identity_hash_code(this);
        match cratonvm_native_api::server_socket_ports::get_addr(identity) {
            Some((host, port)) => (port, host),
            None => (
                cratonvm_native_api::server_socket_ports::get(identity).unwrap_or(0),
                "0.0.0.0".to_string(),
            ),
        }
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
        // Plain ServerSocket — bound iff the binder recorded a port (BUG-04).
        let bound =
            cratonvm_native_api::server_socket_ports::get(ctx.identity_hash_code(this)).is_some();
        return Ok(Some(Value::Int(if bound { 1 } else { 0 })));
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
    Ok(Some(Value::Int(0)))
}

fn ss_wrapper_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if let Some(ssc) = ss_back_ref(ctx, this) {
        let _ = ssc_close(ctx, &[Value::Object(Some(ssc))])?;
        // C27: remove the identity-hashed key (was raw pointer before).
        let key = ctx.identity_hash_code(this);
        ss_back_ref_table().write().remove(&key);
        return Ok(None);
    }
    // Plain ServerSocket (no ServerSocketChannel back-ref). This native is the
    // last-registered — and therefore winning — `close`, but the listener it
    // must drop lives in native-builtins' `s2` registry (the binding went
    // through the plain-bind hook). Delegate through the cross-crate close hook
    // it installs; previously this no-opped, so the listener stayed registered
    // and a thread blocked in ServerSocket.accept() never woke — okhttp's
    // MockWebServer.close() then threw `AssertionError: Gave up waiting for
    // queue to shut down` on teardown.
    if let Some(cb) = cratonvm_native_api::plain_server_socket_close::get() {
        return cb(ctx, args);
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

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
        let accept_listener = {
            let regs = tcp_registry().read();
            match regs.get(&id) {
                Some(TcpHandle::Listener(l)) => l.try_clone().unwrap(),
                _ => panic!("listener must be registered"),
            }
        };

        let waiter = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let err = accept_close_aware(&accept_listener, id, true).unwrap_err();
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
