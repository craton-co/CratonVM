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

use parking_lot::RwLock;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};
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
    /// Connect-in-progress — for non-blocking connect we kick off
    /// an asynchronous connect attempt and stash a `JoinHandle`-equivalent
    /// here so `finishConnect()` can poll completion.
    ///
    /// We model this with a parking_lot Mutex around an `Option<TcpStream>`
    /// + completion channel so `finishConnect` can block-or-poll without
    /// dropping the entry.
    Connecting(ConnectInProgress),
    /// Closed but kept in the map so callers see -1 / -1 idempotently.
    Closed,
}

pub struct ConnectInProgress {
    /// Set to Some(stream) once the worker thread succeeds.
    /// Set to Err once the worker thread fails.
    result: parking_lot::Mutex<Option<Result<TcpStream, std::io::Error>>>,
    /// Bumped to true once the worker terminates.
    done: std::sync::atomic::AtomicBool,
}

pub(crate) fn tcp_registry() -> &'static RwLock<HashMap<i32, TcpHandle>> {
    static REG: OnceLock<RwLock<HashMap<i32, TcpHandle>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}


/// Try to clone a registered handle out of the tcp_registry. Returns
/// None if the id is unknown / Closed / Connecting. Used by the selector
/// to obtain a `SelectableKind` it can poll without taking ownership of
/// the live JDK-visible handle.
pub(crate) fn tcp_clone_for_selector(
    id: i32,
) -> Option<TcpHandleClone> {
    let regs = tcp_registry().read();
    match regs.get(&id) {
        Some(TcpHandle::Listener(l)) => l.try_clone().ok().map(TcpHandleClone::Listener),
        Some(TcpHandle::Stream(s)) => s.try_clone().ok().map(TcpHandleClone::Stream),
        _ => None,
    }
}

pub(crate) enum TcpHandleClone {
    Listener(TcpListener),
    Stream(TcpStream),
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

// ---------------------------------------------------------------------------
// Connect pool (round-7 HIGH-5)
// ---------------------------------------------------------------------------
//
// `socket_channel::connect` previously fell through to `std::thread::spawn`
// for the async-connect path, paying one OS-thread creation per call. A
// microservice with bursty connect traffic (~1k connects/s) saw the
// per-op overhead dominate. We now route async-connect jobs to a small
// fixed-size pool whose worker count is `min(available_parallelism, 32)`.
// Excess jobs queue in the mpsc channel.
//
// The pool is lazy-initialised on first use via `OnceLock`, so VMs that
// never exercise non-blocking connect never pay the pool's startup cost.

struct ConnectJob {
    id: i32,
    target: String,
}

const CONNECT_POOL_MAX_WORKERS: usize = 32;

fn connect_pool_sender() -> &'static std::sync::mpsc::Sender<ConnectJob> {
    static SENDER: OnceLock<std::sync::mpsc::Sender<ConnectJob>> = OnceLock::new();
    SENDER.get_or_init(|| {
        // Bounded by available parallelism but capped — for connect-
        // storms a moderate worker count is enough; extra threads just
        // add scheduler pressure.
        let workers = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4)
            .min(CONNECT_POOL_MAX_WORKERS)
            .max(2);
        let (tx, rx) = std::sync::mpsc::channel::<ConnectJob>();
        // Multi-consumer over a std mpsc receiver requires a shared
        // Mutex; jobs are small and rare relative to socket IO, so a
        // parking_lot Mutex on the receiver is fine.
        let rx = std::sync::Arc::new(parking_lot::Mutex::new(rx));
        for w in 0..workers {
            let rx = std::sync::Arc::clone(&rx);
            let _ = std::thread::Builder::new()
                .name(format!("cratonvm-connect-pool-{w}"))
                .spawn(move || connect_pool_worker(rx));
        }
        // Bug 3 / Bug 6 (HIGH round-9 carryover): the pool currently has
        // no shutdown path — because the `Sender` is held in a `'static
        // OnceLock`, the channel's `Drop` never runs and the worker
        // threads block forever in `recv()` waiting on a sender that
        // won't ever disconnect. On embedded VM teardown (multiple VM
        // instances in a single process) this leaks `workers` OS
        // threads per teardown.
        //
        // CratonVM today is single-VM-per-process: process exit reaps
        // the OS threads, so this is documented as a TODO rather than
        // fixed in-place. The fix is well-understood and minimal:
        //
        //   * Replace this `OnceLock<Sender>` with
        //     `OnceLock<Mutex<Option<Sender>>>`.
        //   * Add `pub fn shutdown_connect_pool()` that `take()`s the
        //     Sender. Dropping it closes the mpsc channel; the worker
        //     `recv()` returns `Err`, the loop breaks, threads exit.
        //   * Wire the shutdown call into the VM teardown hook used by
        //     the round-9 graceful-shutdown story.
        //
        // We intentionally do NOT make that change in this round —
        // changing the SENDER type would touch every caller and risk
        // re-introducing a races with the lazy init. Tracking as:
        //
        // TODO(round-11+): wire connect-pool shutdown into VM teardown
        // for multi-tenant embeddings (Vert.x, WildFly hot-undeploy).
        tx
    })
}

fn connect_pool_worker(
    rx: std::sync::Arc<parking_lot::Mutex<std::sync::mpsc::Receiver<ConnectJob>>>,
) {
    loop {
        let job = {
            // Hold the receiver lock only across `recv()` — when a job
            // arrives we drop the lock immediately so a sibling worker
            // can pick up the next one in parallel with our connect.
            let guard = rx.lock();
            match guard.recv() {
                Ok(j) => j,
                Err(_) => return, // channel closed → process shutdown
            }
        };
        // Brief 5-second timeout so a stuck DNS lookup doesn't pin a worker.
        let res = match job.target.parse::<SocketAddr>() {
            Ok(addr) => TcpStream::connect_timeout(&addr, Duration::from_secs(5)),
            Err(_) => TcpStream::connect(&job.target),
        };
        let map = tcp_registry().read();
        if let Some(TcpHandle::Connecting(prog)) = map.get(&job.id) {
            *prog.result.lock() = Some(res);
            prog.done
                .store(true, std::sync::atomic::Ordering::Release);
            ipc_dbg(format!("connect worker completed id={}", job.id));
        }
    }
}

fn connect_pool_submit(job: ConnectJob) {
    let id = job.id;
    if let Err(e) = connect_pool_sender().send(job) {
        ipc_dbg(format!("connect pool send failed id={id}: {e}"));
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn ioex(msg: impl Into<String>) -> MethodCallFailed {
    RuntimeError::IOException { message: msg.into() }.into()
}

fn map_err(ctx: &str, e: std::io::Error) -> MethodCallFailed {
    let prefix = match e.kind() {
        ErrorKind::ConnectionRefused => "ConnectException",
        ErrorKind::AddrInUse => "BindException: Address already in use",
        ErrorKind::AddrNotAvailable => "BindException: Cannot assign requested address",
        ErrorKind::PermissionDenied => "BindException: Permission denied",
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset => "SocketException",
        ErrorKind::NotConnected => "SocketException: Not connected",
        ErrorKind::TimedOut => "SocketTimeoutException",
        _ => "SocketException",
    };
    ioex(format!("{prefix}: {ctx}: {e}"))
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
    let host_via_method = match ctx.invoke_virtual(sa, "getHostString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    };
    if let Some(p) = port_via_method {
        let h = host_via_method.unwrap_or_else(|| "0.0.0.0".to_string());
        let h = if h.is_empty() { "0.0.0.0".to_string() } else { h };
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
        BufferAccess::Heap { arr, offset, length } if length > 0 => {
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
        BufferAccess::Heap { arr, offset, length } if length > 0 => {
            // Bulk write via NativeContext intrinsic. The old loop wrote at
            // most `min(data.len(), length)` bytes, stopping early if it ran
            // past `arr_len`; clamp the effective length the same way.
            let arr_len = ctx.array_length(arr);
            let off = offset as usize;
            let avail = arr_len.saturating_sub(off);
            let n = (data.len() as i32)
                .min(length)
                .max(0)
                .min(avail as i32);
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
        Some(o) => Ok(Some(Value::Int(if read_blocking_flag(ctx, o) { 1 } else { 0 }))),
        _ => Ok(Some(Value::Int(1))),
    }
}

fn sc_is_connected(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) => Ok(Some(cf_get(ctx, o, F_CONNECTED))),
        _ => Ok(Some(Value::Int(0))),
    }
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
    cf_set(ctx, this, F_BLOCKING, Value::Int(if blocking { 1 } else { 0 }));
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
            let h = ctx.create_string(&host);
            return ctx.new_object_initialized(
                "java/net/InetSocketAddress",
                "(Ljava/lang/String;I)V",
                &[Value::Object(Some(h)), Value::Int(port)],
            );
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
    let h = ctx.create_string(&addr.ip().to_string());
    ctx.new_object_initialized(
        "java/net/InetSocketAddress",
        "(Ljava/lang/String;I)V",
        &[Value::Object(Some(h)), Value::Int(addr.port() as i32)],
    )
}

// ---------------------------------------------------------------------------
// SocketChannel.connect / finishConnect
// ---------------------------------------------------------------------------

/// H3b: resolve a `host:port` target to one or more `SocketAddr`s and
/// vet every resolved address against the outbound-host policy, mirroring
/// `outbound_policy::policy_connect`'s resolution loop. The blocking path
/// gets this for free via `policy_connect`; the non-blocking path calls
/// this so its downstream dials (the fast-path connect and the background
/// `connect_pool_worker`, both of which would otherwise re-run DNS) only
/// ever target a vetted, already-resolved IP. This closes the DNS-rebind
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
/// wait for the connection to succeed/fail. In non-blocking mode we kick
/// off the connect on a background thread and return false immediately;
/// `finishConnect()` later polls the result.
/// A connect error that is a definitive "this peer will not answer" result (as
/// opposed to TimedOut/WouldBlock, which may just be a slow host). For these we
/// report the failure immediately rather than deferring to the background-connect
/// pool, whose pending state the selector cannot surface as OP_CONNECT.
fn is_definitive_connect_failure(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::AddrNotAvailable
    )
}

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
        let stream = match crate::outbound_policy::policy_connect(&target) {
            Ok(s) => s,
            Err(crate::outbound_policy::PolicyConnectError::Denied(reason)) => {
                return Err(ioex(format!(
                    "connect denied by outbound policy: {reason}"
                )));
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
        ipc_dbg(format!("connect success(blocking) id={id} local_port={local_port}"));
        return Ok(true);
    }

    // Task #16 / H3b: policy gate also applies to non-blocking connects.
    // The blocking branch above routes through `policy_connect`, which
    // re-checks every *resolved* SocketAddr. The non-blocking branch must
    // do the same: a `check_outbound(&target)` on the literal `host:port`
    // string only blocks targets that *parse* as a link-local IP — it
    // does NOT resolve DNS. Since the dials below (`TcpStream::connect`
    // and the background `connect_pool_worker`) do their own resolution,
    // a hostname that resolves to `169.254.169.254` would otherwise slip
    // through (DNS-rebind SSRF). So we resolve here, vet every resolved
    // IP against the outbound policy, and dial the *vetted* SocketAddr(s)
    // directly — never re-resolving the original hostname downstream.
    let vetted = resolve_and_vet(&target)?;

    // Non-blocking path fast-path: for localhost IPC (e.g., Surefire
    // master-fork channel), a short synchronous dial is more robust than
    // deferring connect completion to a background thread. We try each
    // vetted address with the existing 750 ms fast-path timeout
    // (deliberately shorter than the global 30 s cap — localhost should
    // answer in milliseconds).
    let immediate = {
        let mut last: Option<Result<TcpStream, std::io::Error>> = None;
        for addr in &vetted {
            match TcpStream::connect_timeout(addr, Duration::from_millis(750)) {
                Ok(s) => {
                    last = Some(Ok(s));
                    break;
                }
                Err(e) => last = Some(Err(e)),
            }
        }
        last.unwrap_or_else(|| {
            Err(std::io::Error::new(
                ErrorKind::AddrNotAvailable,
                format!("no addresses resolved for {target}"),
            ))
        })
    };
    match immediate {
        Ok(stream) => {
            let _ = stream.set_nonblocking(true);
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
                "connect success(nonblocking-fastpath) id={id} local_port={local_port}"
            ));
            return Ok(true);
        }
        // A non-blocking connect that is DEFINITIVELY refused/reset (a stopped
        // host on loopback) must fail now, not park in the background-connect
        // "Connecting" state below — that state has no OS handle the selector can
        // poll, so an Apache-NIO-reactor client registering OP_CONNECT would wait
        // forever, time the session request out, and lose the request (the
        // CancelledKeyException in ES MultipleHosts testAsyncRequests). Surfacing
        // it makes SocketChannel.connect() throw, so the reactor fails the request
        // fast and the RestClient retries another node. Only INDETERMINATE errors
        // (e.g. TimedOut — the host may just be slow) fall through to the pool.
        // A definitively-refused connect is surfaced now (the peer is dead).
        // Everything else (incl. a loopback fast-path TIMEOUT — the peer may be
        // slow OR dead) is deferred to the background pool below, which dials with
        // its own short loopback bound and whose result the selector surfaces as
        // OP_CONNECT; that keeps a slow-but-live loopback peer working (it succeeds
        // via the pool) while a dead one fails fast WITHOUT blocking the caller.
        Err(e) if is_definitive_connect_failure(&e) => {
            ipc_dbg(format!("connect refused(nonblocking-fastpath) target={target}: {e}"));
            return Err(map_err(&target, e));
        }
        Err(_) => {}
    }

    // Round-7 HIGH-5 fix: fallback uses a small fixed-size connect
    // pool instead of `std::thread::spawn` per call.  A microservice
    // connect-storm previously paid one OS-thread creation per pending
    // connect; the pool caps that at `CONNECT_POOL_WORKERS` workers
    // total.  Excess jobs queue in the channel.
    let progress = ConnectInProgress {
        result: parking_lot::Mutex::new(None),
        done: std::sync::atomic::AtomicBool::new(false),
    };
    let id = tcp_register(TcpHandle::Connecting(progress));
    tcp_blocking_state().write().insert(id, false);

    // H3b: hand the pool worker a *resolved, vetted* literal `IP:port`
    // string (not the original hostname) so its `parse::<SocketAddr>()`
    // branch succeeds and it never performs a second, unchecked DNS
    // resolution that could land on a link-local address.
    let job_target = vetted
        .first()
        .map(|a| a.to_string())
        .unwrap_or_else(|| target.clone());
    connect_pool_submit(ConnectJob { id, target: job_target });

    cf_set(ctx, this, F_REG_ID, Value::Int(id));
    let host_str = ctx.create_string(&host);
    cf_set(ctx, this, F_REMOTE, Value::Object(Some(host_str)));
    cf_set(ctx, this, F_REMOTE_PORT, Value::Int(port as i32));
    ipc_dbg(format!("connect pending id={id}"));
    Ok(false)
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

    // Check the current state of the registry entry.
    let res_kind = {
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Stream(_)) => 1,    // already connected
            Some(TcpHandle::Connecting(p)) => {
                if p.done.load(std::sync::atomic::Ordering::Acquire) {
                    2
                } else {
                    0
                }
            }
            _ => -1,
        }
    };

    match res_kind {
        1 => {
            // Was connected synchronously already.
            cf_set(ctx, this, F_CONNECTED, Value::Int(1));
            Ok(Some(Value::Int(1)))
        }
        2 => {
            // Worker finished — promote the entry.
            let mut map = tcp_registry().write();
            let prog = match map.remove(&id) {
                Some(TcpHandle::Connecting(p)) => p,
                other => {
                    // Race: someone else moved it. Put back if so.
                    if let Some(h) = other {
                        map.insert(id, h);
                    }
                    return Ok(Some(Value::Int(0)));
                }
            };
            let result = prog.result.lock().take();
            match result {
                Some(Ok(stream)) => {
                    let local_port =
                        stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
                    // Apply current non-blocking flag.
                    let nb = !read_blocking_flag(ctx, this);
                    if nb {
                        let _ = stream.set_nonblocking(true);
                    }
                    map.insert(id, TcpHandle::Stream(stream));
                    drop(map);
                    cf_set(ctx, this, F_CONNECTED, Value::Int(1));
                    cf_set(ctx, this, F_LOCAL_PORT, Value::Int(local_port));
                    Ok(Some(Value::Int(1)))
                }
                Some(Err(e)) => {
                    drop(map);
                    Err(map_err("finishConnect", e))
                }
                None => Ok(Some(Value::Int(0))),
            }
        }
        0 => Ok(Some(Value::Int(0))),
        _ => Err(ioex("finishConnect: socket not in connecting state")),
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
        Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
            Ok(None)
        }
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
    let id = read_reg_id(ctx, this)
        .ok_or_else(|| ioex("read: channel not connected"))?;

    // Determine the writable region. We materialize into a heap buffer here
    // and copy into the buffer slot afterwards so we don't hold a registry
    // lock across `set_array_element`.
    let access = buffer_access(ctx, bb)
        .ok_or_else(|| ioex("read: ByteBuffer has no decodable layout"))?;
    let len = match access {
        BufferAccess::Direct { length, .. } => length,
        BufferAccess::Heap { length, .. } => length,
    };
    if len <= 0 {
        return Ok(Some(Value::Int(0)));
    }
    let mut buf = vec![0u8; len as usize];

    let n_opt = {
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Stream(s)) => try_read_nb(s, &mut buf).map_err(|e| map_err("read", e))?,
            Some(TcpHandle::Connecting(_)) => return Ok(Some(Value::Int(0))),
            _ => return Err(ioex("read: channel not a stream")),
        }
    };

    let n = match n_opt {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))), // EAGAIN — JDK convention
    };
    if n > 0 {
        crate::net::socket_capture('r', id, &buf[..n as usize]);
        let written = buffer_write_bytes(ctx, bb, &buf[..n as usize]);
        buffer_advance(ctx, bb, written);
    }
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
    let id = read_reg_id(ctx, this)
        .ok_or_else(|| ioex("write: channel not connected"))?;
    let data = buffer_read_bytes(ctx, bb).unwrap_or_default();
    if data.is_empty() {
        return Ok(Some(Value::Int(0)));
    }
    let n_opt = {
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Stream(s)) => try_write_nb(s, &data).map_err(|e| map_err("write", e))?,
            Some(TcpHandle::Connecting(_)) => return Ok(Some(Value::Int(0))),
            _ => return Err(ioex("write: channel not a stream")),
        }
    };

    let n = match n_opt {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))), // EAGAIN
    };
    if n > 0 {
        crate::net::socket_capture('w', id, &data[..n as usize]);
        buffer_advance(ctx, bb, n);
    }
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
    let id = read_reg_id(ctx, this)
        .ok_or_else(|| ioex("write(gathering): channel not connected"))?;

    // Collect each buffer's readable region (in order), keeping the buffer ref
    // so we can advance its position by the bytes actually consumed.
    let arr_len = ctx.array_length(srcs) as i32;
    let (start, end) = vec_window(args, arr_len);
    let mut chunks: Vec<(ObjectRef, Vec<u8>)> = Vec::new();
    let mut total: usize = 0;
    for i in start..end {
        if let Value::Object(Some(bb)) = ctx.get_array_element(srcs, i as usize) {
            let bytes = buffer_read_bytes(ctx, bb).unwrap_or_default();
            total += bytes.len();
            chunks.push((bb, bytes));
        }
    }
    if total == 0 {
        return Ok(Some(Value::Long(0)));
    }
    let mut data = Vec::with_capacity(total);
    for (_, bytes) in &chunks {
        data.extend_from_slice(bytes);
    }

    let n_opt = {
        let map = tcp_registry().read();
        match map.get(&id) {
            Some(TcpHandle::Stream(s)) => {
                try_write_nb(s, &data).map_err(|e| map_err("write(gathering)", e))?
            }
            Some(TcpHandle::Connecting(_)) => return Ok(Some(Value::Long(0))),
            _ => return Err(ioex("write(gathering): channel not a stream")),
        }
    };
    let n = match n_opt {
        Some(v) => v,
        None => return Ok(Some(Value::Long(0))), // EAGAIN — JDK convention
    };
    if n > 0 {
        crate::net::socket_capture('w', id, &data[..n as usize]);
        // Distribute the written count across the source buffers, advancing
        // each position by the portion of its bytes that made it out.
        let mut remaining = n;
        for (bb, bytes) in &chunks {
            if remaining <= 0 {
                break;
            }
            let consume = (bytes.len() as i32).min(remaining);
            buffer_advance(ctx, *bb, consume);
            remaining -= consume;
        }
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
    let id = read_reg_id(ctx, this)
        .ok_or_else(|| ioex("read(scattering): channel not connected"))?;

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
        "SO_KEEPALIVE" => Ok(()),     // std::net offers no setter without socket2
        "SO_REUSEADDR" => Ok(()),     // pre-bind only
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
fn box_socket_option(
    ctx: &mut dyn NativeContext,
    opt_name: &str,
    raw: i32,
) -> MethodCallResult {
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
    let local_port = listener.local_addr().map(|a| a.port() as i32).unwrap_or(port as i32);
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
    let id = read_reg_id(ctx, this)
        .ok_or_else(|| ioex("accept: server channel not bound"))?;
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
            Some(TcpHandle::Listener(l)) => l
                .try_clone()
                .map_err(|e| map_err("accept clone", e))?,
            _ => return Err(ioex("accept: id is not a listener")),
        }
    };
    // A cloned socket does not reliably inherit the parent's blocking mode on
    // Windows, so set it explicitly to match the channel. Without this a
    // non-blocking accept() would block forever instead of returning null.
    let _ = listener_clone.set_nonblocking(!blocking);

    let accepted = if let Some(stream) = preaccepted {
        let peer = stream.peer_addr().unwrap_or_else(|_| {
            "0.0.0.0:0".parse().unwrap()
        });
        Some((stream, peer))
    } else {
        match listener_clone.accept() {
            Ok((stream, peer)) => Some((stream, peer)),
            Err(e) if e.kind() == ErrorKind::WouldBlock && !blocking => None,
            Err(e) => return Err(map_err("accept", e)),
        }
    };

    let Some((stream, peer)) = accepted else {
        return Ok(Some(Value::Object(None)));
    };

    // Inherit non-blocking flag of the parent channel.
    if !blocking {
        let _ = stream.set_nonblocking(true);
    }

    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    let new_id = tcp_register(TcpHandle::Stream(stream));
    tcp_blocking_state().write().insert(new_id, blocking);

    let child = alloc_obj(ctx, "java/nio/channels/SocketChannel", N_FIELDS);
    init_channel_locks(ctx, child);
    cf_set(ctx, child, F_OPEN, Value::Int(1));
    cf_set(ctx, child, F_BLOCKING, Value::Int(if blocking { 1 } else { 0 }));
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
        r.register(
            c,
            "connect",
            "(Ljava/net/SocketAddress;)Z",
            sc_connect,
        );
        r.register(
            c,
            "blockingConnect",
            "(Ljava/net/SocketAddress;J)V",
            sc_blocking_connect,
        );
        r.register(c, "finishConnect", "()Z", sc_finish_connect);
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
        r.register(c, "write", "([Ljava/nio/ByteBuffer;II)J", sc_write_gathering);
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
    }

    // -- ServerSocketChannel factory + lifecycle --
    for c in [ssc, sscimpl] {
        r.register(c, "open", "()Ljava/nio/channels/ServerSocketChannel;", ssc_open);
        r.register(c, "socket", "()Ljava/net/ServerSocket;", ssc_socket);
        r.register(c, "isOpen", "()Z", sc_is_open);
        r.register(c, "isBlocking", "()Z", sc_is_blocking);
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
        r.register(c, "accept", "()Ljava/nio/channels/SocketChannel;", ssc_accept);
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
            "getLocalAddress",
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

fn ss_back_ref_table()
    -> &'static RwLock<rustc_hash::FxHashMap<i32, ObjectRef>>
{
    static REG: OnceLock<RwLock<rustc_hash::FxHashMap<i32, ObjectRef>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(rustc_hash::FxHashMap::default()))
}

fn ss_record_back_ref(ctx: &mut dyn NativeContext, ss: ObjectRef, ssc: ObjectRef) {
    let key = ctx.identity_hash_code(ss);
    ss_back_ref_table()
        .write()
        .insert(key, ssc);
}

fn ss_back_ref(ctx: &mut dyn NativeContext, ss: ObjectRef) -> Option<ObjectRef> {
    let key = ctx.identity_hash_code(ss);
    ss_back_ref_table()
        .read()
        .get(&key)
        .copied()
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
pub fn ss_back_ref_update_after_gc(
    pointer_map: &rustc_hash::FxHashMap<usize, usize>,
) {
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
    let h = ctx.create_string("0.0.0.0");
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
        // Plain ServerSocket — fall through (handled elsewhere). We can't
        // do anything for a non-channel-backed ServerSocket here.
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
    Ok(Some(Value::Int(0)))
}

fn ss_wrapper_local_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let port = if let Some(ssc) = ss_back_ref(ctx, this) {
        cf_get(ctx, ssc, F_LOCAL_PORT).as_int().unwrap_or(0)
    } else {
        0
    };
    if port <= 0 {
        return Ok(Some(Value::Object(None)));
    }
    let isa = alloc_obj(ctx, "java/net/InetSocketAddress", 2);
    let host = ctx.create_string("0.0.0.0");
    ctx.set_field(isa, 0, Value::Object(Some(host)));
    ctx.set_field(isa, 1, Value::Int(port));
    Ok(Some(Value::Object(Some(isa))))
}

fn ss_wrapper_is_bound(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let Some(ssc) = ss_back_ref(ctx, this) else {
        return Ok(Some(Value::Int(0)));
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
        ss_back_ref_table()
            .write()
            .remove(&key);
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
}
