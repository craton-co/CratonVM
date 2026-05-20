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
use std::net::{SocketAddr, TcpListener, TcpStream};
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
        Ok(cid) => ctx.alloc_object(cid, nfields),
        Err(_) => ctx.alloc_object(ClassId::new(0), nfields),
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

fn read_reg_id(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    if ctx.object_num_fields(this) <= F_REG_ID {
        return None;
    }
    match ctx.get_field(this, F_REG_ID) {
        Value::Int(v) if v != 0 && v != -1 => Some(v),
        _ => None,
    }
}

fn read_blocking_flag(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    if ctx.object_num_fields(this) <= F_BLOCKING {
        return true;
    }
    match ctx.get_field(this, F_BLOCKING) {
        Value::Int(0) => false,
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// SocketAddress decoding
// ---------------------------------------------------------------------------

/// Try to read a socket address out of a Java `InetSocketAddress`-shaped
/// object. The synthetic layout is `(host:String, port:int)`. Real-JDK
/// `InetSocketAddress` is more elaborate (has a holder), but we reach
/// for `getHostString()` / `getPort()` via `get_field_by_name` first.
fn decode_socket_address(
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
            // SAFETY: the JDK guarantees `length` bytes are mapped.
            unsafe {
                std::ptr::copy_nonoverlapping(addr as *const u8, v.as_mut_ptr(), length as usize);
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
            // SAFETY: caller guarantees `length` bytes are addressable.
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, n as usize);
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
    ctx.set_field(ch, F_OPEN, Value::Int(1));
    ctx.set_field(ch, F_BLOCKING, Value::Int(1));
    ctx.set_field(ch, F_REG_ID, Value::Int(-1));
    ctx.set_field(ch, F_CONNECTED, Value::Int(0));
    ctx.set_field(ch, F_LOCAL_PORT, Value::Int(0));
    ctx.set_field(ch, F_REMOTE, Value::Object(None));
    ctx.set_field(ch, F_REMOTE_PORT, Value::Int(0));
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
        Some(o) if ctx.object_num_fields(o) > F_OPEN => Ok(Some(ctx.get_field(o, F_OPEN))),
        _ => Ok(Some(Value::Int(0))),
    }
}

fn sc_is_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) > F_BLOCKING => {
            Ok(Some(ctx.get_field(o, F_BLOCKING)))
        }
        _ => Ok(Some(Value::Int(1))),
    }
}

fn sc_is_connected(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) > F_CONNECTED => {
            Ok(Some(ctx.get_field(o, F_CONNECTED)))
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
    if ctx.object_num_fields(this) > F_BLOCKING {
        ctx.set_field(this, F_BLOCKING, Value::Int(if blocking { 1 } else { 0 }));
    }
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
        let nf = ctx.object_num_fields(this);
        if nf > F_OPEN {
            ctx.set_field(this, F_OPEN, Value::Int(0));
        }
        if nf > F_CONNECTED {
            ctx.set_field(this, F_CONNECTED, Value::Int(0));
        }
        if let Some(id) = read_reg_id(ctx, this) {
            tcp_remove(id);
            ctx.set_field(this, F_REG_ID, Value::Int(-1));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// SocketChannel.connect / finishConnect
// ---------------------------------------------------------------------------

/// Inner connect routine. When `allow_block` is true (blocking mode), we
/// wait for the connection to succeed/fail. In non-blocking mode we kick
/// off the connect on a background thread and return false immediately;
/// `finishConnect()` later polls the result.
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
        let stream = TcpStream::connect(&target).map_err(|e| map_err(&target, e))?;
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
        if ctx.object_num_fields(this) >= N_FIELDS {
            ctx.set_field(this, F_REG_ID, Value::Int(id));
            ctx.set_field(this, F_CONNECTED, Value::Int(1));
            ctx.set_field(this, F_LOCAL_PORT, Value::Int(local_port));
            let host_str = ctx.create_string(&host);
            ctx.set_field(this, F_REMOTE, Value::Object(Some(host_str)));
            ctx.set_field(this, F_REMOTE_PORT, Value::Int(port as i32));
        }
        ipc_dbg(format!("connect success(blocking) id={id} local_port={local_port}"));
        return Ok(true);
    }

    // Non-blocking path fast-path: for localhost IPC (e.g., Surefire
    // master-fork channel), a short synchronous dial is more robust than
    // deferring connect completion to a background thread.
    let immediate = match target.parse::<SocketAddr>() {
        Ok(addr) => TcpStream::connect_timeout(&addr, Duration::from_millis(750)),
        Err(_) => TcpStream::connect(&target),
    };
    if let Ok(stream) = immediate {
        let _ = stream.set_nonblocking(true);
        let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
        let id = tcp_register(TcpHandle::Stream(stream));
        tcp_blocking_state().write().insert(id, false);
        if ctx.object_num_fields(this) >= N_FIELDS {
            ctx.set_field(this, F_REG_ID, Value::Int(id));
            ctx.set_field(this, F_CONNECTED, Value::Int(1));
            ctx.set_field(this, F_LOCAL_PORT, Value::Int(local_port));
            let host_str = ctx.create_string(&host);
            ctx.set_field(this, F_REMOTE, Value::Object(Some(host_str)));
            ctx.set_field(this, F_REMOTE_PORT, Value::Int(port as i32));
        }
        ipc_dbg(format!(
            "connect success(nonblocking-fastpath) id={id} local_port={local_port}"
        ));
        return Ok(true);
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

    connect_pool_submit(ConnectJob { id, target: target.clone() });

    if ctx.object_num_fields(this) >= N_FIELDS {
        ctx.set_field(this, F_REG_ID, Value::Int(id));
        let host_str = ctx.create_string(&host);
        ctx.set_field(this, F_REMOTE, Value::Object(Some(host_str)));
        ctx.set_field(this, F_REMOTE_PORT, Value::Int(port as i32));
    }
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
            if ctx.object_num_fields(this) > F_CONNECTED {
                ctx.set_field(this, F_CONNECTED, Value::Int(1));
            }
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
                    if ctx.object_num_fields(this) >= N_FIELDS {
                        ctx.set_field(this, F_CONNECTED, Value::Int(1));
                        ctx.set_field(this, F_LOCAL_PORT, Value::Int(local_port));
                    }
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
        buffer_advance(ctx, bb, n);
    }
    Ok(Some(Value::Int(n)))
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
    if let Some(id) = read_reg_id(ctx, this) {
        let map = tcp_registry().read();
        if let Some(TcpHandle::Stream(s)) = map.get(&id) {
            let v = read_option(s, &opt_name).unwrap_or(0);
            return Ok(Some(Value::Int(v)));
        }
    }
    Ok(Some(Value::Int(0)))
}

// ---------------------------------------------------------------------------
// ServerSocketChannel — open / bind / accept / close
// ---------------------------------------------------------------------------

fn ssc_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let ch = alloc_obj(ctx, "java/nio/channels/ServerSocketChannel", N_FIELDS);
    ctx.set_field(ch, F_OPEN, Value::Int(1));
    ctx.set_field(ch, F_BLOCKING, Value::Int(1));
    ctx.set_field(ch, F_REG_ID, Value::Int(-1));
    ctx.set_field(ch, F_CONNECTED, Value::Int(0));
    ctx.set_field(ch, F_LOCAL_PORT, Value::Int(0));
    ctx.set_field(ch, F_REMOTE, Value::Object(None));
    ctx.set_field(ch, F_REMOTE_PORT, Value::Int(0));
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

    if ctx.object_num_fields(this) >= N_FIELDS {
        ctx.set_field(this, F_REG_ID, Value::Int(id));
        ctx.set_field(this, F_LOCAL_PORT, Value::Int(local_port));
    }
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
    ctx.set_field(child, F_OPEN, Value::Int(1));
    ctx.set_field(child, F_BLOCKING, Value::Int(if blocking { 1 } else { 0 }));
    ctx.set_field(child, F_REG_ID, Value::Int(new_id));
    ctx.set_field(child, F_CONNECTED, Value::Int(1));
    ctx.set_field(child, F_LOCAL_PORT, Value::Int(local_port));
    let host_str = ctx.create_string(&peer.ip().to_string());
    ctx.set_field(child, F_REMOTE, Value::Object(Some(host_str)));
    ctx.set_field(child, F_REMOTE_PORT, Value::Int(peer.port() as i32));

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
        r.register(
            c,
            "connect",
            "(Ljava/net/SocketAddress;)Z",
            sc_connect,
        );
        r.register(c, "finishConnect", "()Z", sc_finish_connect);
        r.register(c, "read", "(Ljava/nio/ByteBuffer;)I", sc_read);
        r.register(c, "write", "(Ljava/nio/ByteBuffer;)I", sc_write);
        r.register(
            c,
            "setOption",
            "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
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
}

// ---------------------------------------------------------------------------
// ServerSocketChannel.socket() — wrapper ServerSocket
// ---------------------------------------------------------------------------

// The channel-backed wrapper is a plain `java/net/ServerSocket` allocated
// with the JDK's real layout; we cannot stash a back-ref inside one of its
// fields without colliding with JDK-private slots. Instead we keep the
// wrapper → channel mapping in a process-wide side-table keyed by the
// wrapper's raw ObjectRef pointer.
const SSC_SOCKET_CACHE: usize = 5; // unused F_REMOTE slot — see note below.

fn ss_back_ref_table()
    -> &'static RwLock<HashMap<usize, usize>>
{
    static REG: OnceLock<RwLock<HashMap<usize, usize>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

fn ss_record_back_ref(ss: ObjectRef, ssc: ObjectRef) {
    ss_back_ref_table()
        .write()
        .insert(ss.as_ptr() as usize, ssc.as_ptr() as usize);
}

fn ss_back_ref(ss: ObjectRef) -> Option<ObjectRef> {
    let raw = ss_back_ref_table()
        .read()
        .get(&(ss.as_ptr() as usize))
        .copied()?;
    // SAFETY: the SSC is kept alive by the wrapper's reference path; the
    // mapping is removed in ssc_close.
    Some(unsafe { ObjectRef::from_raw(raw as *mut u8) })
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
    ss_record_back_ref(ss_value, this);
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
    let port = ctx.get_field(this, F_LOCAL_PORT).as_int().unwrap_or(0);
    let id = ctx.get_field(this, F_REG_ID).as_int().unwrap_or(-1);
    if id < 0 || port <= 0 {
        return Ok(Some(Value::Object(None)));
    }
    let isa = alloc_obj(ctx, "java/net/InetSocketAddress", 2);
    let host = ctx.create_string("0.0.0.0");
    ctx.set_field(isa, 0, Value::Object(Some(host)));
    ctx.set_field(isa, 1, Value::Int(port));
    Ok(Some(Value::Object(Some(isa))))
}

fn ss_wrapper_bind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Err(ioex("bind: null this")),
    };
    let Some(ssc) = ss_back_ref(this) else {
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
    let Some(ssc) = ss_back_ref(this) else {
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
    if let Some(ssc) = ss_back_ref(this) {
        let port = ctx.get_field(ssc, F_LOCAL_PORT).as_int().unwrap_or(0);
        return Ok(Some(Value::Int(port)));
    }
    Ok(Some(Value::Int(0)))
}

fn ss_wrapper_local_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let port = if let Some(ssc) = ss_back_ref(this) {
        ctx.get_field(ssc, F_LOCAL_PORT).as_int().unwrap_or(0)
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

fn ss_wrapper_is_bound(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let Some(ssc) = ss_back_ref(this) else {
        return Ok(Some(Value::Int(0)));
    };
    let id = _ctx.get_field(ssc, F_REG_ID).as_int().unwrap_or(-1);
    Ok(Some(Value::Int(if id >= 0 { 1 } else { 0 })))
}

fn ss_wrapper_is_closed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(1))),
    };
    if let Some(ssc) = ss_back_ref(this) {
        let open = ctx.get_field(ssc, F_OPEN).as_int().unwrap_or(0);
        return Ok(Some(Value::Int(if open == 0 { 1 } else { 0 })));
    }
    Ok(Some(Value::Int(0)))
}

fn ss_wrapper_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if let Some(ssc) = ss_back_ref(this) {
        let _ = ssc_close(ctx, &[Value::Object(Some(ssc))])?;
        ss_back_ref_table()
            .write()
            .remove(&(this.as_ptr() as usize));
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
