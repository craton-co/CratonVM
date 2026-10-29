// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Windows: the natives the JDK's own asynchronous channels (`sun.nio.ch.Iocp`,
//! `WindowsAsynchronousSocketChannelImpl`, `WindowsAsynchronousServerSocketChannelImpl`,
//! `WindowsAsynchronousFileChannelImpl`) end in, over an in-process completion-port emulation.
//!
//! HotSpot posts an overlapped `WSARecv` / `WSASend` / `AcceptEx` / `ConnectEx` / `ReadFile` /
//! `WriteFile` / `LockFileEx` to the kernel and a pool of threads blocks in `GetQueuedCompletionStatus`.
//! Here a port is a queue with a condition variable; each operation is started by the native (which
//! still has the VM context, so it reads the `WSABUF` array and the source bytes of a write at call
//! time) and run by a worker thread that owns plain Rust data only. The worker posts one completion
//! `(key, OVERLAPPED address, error, bytes)`; a read's bytes ride in the completion and are copied into
//! the Java buffers by `getQueuedCompletionStatus`, which is the one place with a VM context again.
//! Every operation answers `IOStatus.UNAVAILABLE` ("pending"), the contract of an overlapped call that
//! completes later: the JDK handles the immediate-completion arm too, but never needs it.
//!
//! `closesocket0` marks the socket aborted and shuts it down, so a blocked worker returns and posts
//! `ERROR_OPERATION_ABORTED`, which is what `PendingIoCache.close` waits for.
//!
//! A child module of `net` so it shares that file's private socket registry.

use super::*;

use parking_lot::Condvar;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

/// Port handles; kept apart from socket ids (`0x4000_0000..`) and file-table ids.
const PORT_BASE: i64 = 0x5000_0000;
const INVALID_HANDLE_VALUE: i64 = -1;
const IOS_UNAVAILABLE: i32 = -2;

const ERROR_HANDLE_EOF: i32 = 38;
const ERROR_OPERATION_ABORTED: i32 = 995;
const WSAENOTCONN: i32 = 10057;
const WSAECONNRESET: i32 = 10054;
const WSAECONNREFUSED: i32 = 10061;
const WSAEACCES: i32 = 10013;

/// What a read's worker hands back for `getQueuedCompletionStatus` to scatter into the Java buffers.
struct ReadBack {
    data: Vec<u8>,
    /// `(address, length)` of each `WSABUF` (or the one file buffer), in order.
    dests: Vec<(i64, usize)>,
}

struct Completion {
    key: i32,
    overlapped: i64,
    error: i32,
    bytes: i32,
    read: Option<ReadBack>,
}

struct Port {
    queue: Mutex<VecDeque<Completion>>,
    ready: Condvar,
    closed: AtomicBool,
}

fn ports() -> &'static Mutex<HashMap<i64, Arc<Port>>> {
    static T: OnceLock<Mutex<HashMap<i64, Arc<Port>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `handle (socket id or file-table id) -> (port, completion key)`.
fn associations() -> &'static Mutex<HashMap<i64, (i64, i32)>> {
    static T: OnceLock<Mutex<HashMap<i64, (i64, i32)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Sockets `closesocket0` has been called on: their pending operations complete as aborted.
fn aborted() -> &'static Mutex<HashSet<i64>> {
    static T: OnceLock<Mutex<HashSet<i64>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashSet::new()))
}

fn next_port() -> i64 {
    static N: AtomicI64 = AtomicI64::new(0);
    PORT_BASE + N.fetch_add(1, Ordering::Relaxed)
}

fn port_of(handle: i64) -> Option<Arc<Port>> {
    ports().lock().get(&handle).cloned()
}

fn long_at(args: &[Value], idx: usize) -> i64 {
    match args.get(idx) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => i64::from(*v),
        _ => 0,
    }
}

/// Post `c` to the port `handle` names; a closed or unknown port drops it.
fn post(handle: i64, c: Completion) {
    if let Some(p) = port_of(handle) {
        if p.closed.load(Ordering::Acquire) {
            return;
        }
        p.queue.lock().push_back(c);
        p.ready.notify_one();
    }
}

/// Post the completion of an operation on `handle` (looked up in the association table), `overlapped`
/// being the OVERLAPPED address the JDK keyed its pending result by.
fn complete(handle: i64, overlapped: i64, error: i32, bytes: i32, read: Option<ReadBack>) {
    let assoc = associations().lock().get(&handle).copied();
    if let Some((port, key)) = assoc {
        post(
            port,
            Completion {
                key,
                overlapped,
                error,
                bytes,
                read,
            },
        );
    }
}

fn is_aborted(handle: i64) -> bool {
    aborted().lock().contains(&handle)
}

/// The Win32 code an overlapped completion carries for the Winsock error `wsa`: the kernel completes a
/// `ConnectEx` / `WSARecv` with the NTSTATUS-translated code, not the `WSAE*` one (a refused connect is
/// `ERROR_CONNECTION_REFUSED`, 1225; the message the JDK shows is that code's).
fn overlapped_error(wsa: i32) -> i32 {
    match wsa {
        10061 => 1225, // WSAECONNREFUSED -> ERROR_CONNECTION_REFUSED
        10060 => 121,  // WSAETIMEDOUT -> ERROR_SEM_TIMEOUT
        10051 => 1231, // WSAENETUNREACH -> ERROR_NETWORK_UNREACHABLE
        10065 => 1232, // WSAEHOSTUNREACH -> ERROR_HOST_UNREACHABLE
        10054 => 64,   // WSAECONNRESET -> ERROR_NETNAME_DELETED
        10053 => 1236, // WSAECONNABORTED -> ERROR_CONNECTION_ABORTED
        other => other,
    }
}

/// The Windows error code a worker reports for `e`.
fn error_code(e: &std::io::Error, handle: i64) -> i32 {
    if is_aborted(handle) {
        return ERROR_OPERATION_ABORTED;
    }
    overlapped_error(match e.raw_os_error() {
        Some(code) if code != 0 => code,
        _ => match e.kind() {
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
                WSAECONNRESET
            }
            std::io::ErrorKind::ConnectionRefused => WSAECONNREFUSED,
            std::io::ErrorKind::PermissionDenied => WSAEACCES,
            std::io::ErrorKind::NotConnected => WSAENOTCONN,
            _ => ERROR_OPERATION_ABORTED,
        },
    })
}

fn spawn<F: FnOnce() + Send + 'static>(f: F) {
    let _ = std::thread::Builder::new()
        .name("cratonvm-iocp-op".into())
        .spawn(f);
}

// ---------------------------------------------------------------------------------------------
// sun.nio.ch.Iocp
// ---------------------------------------------------------------------------------------------

fn iocp_init_ids(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// `createIoCompletionPort(handle, existingPort, completionKey, concurrency) -> port`: a new port for
/// `INVALID_HANDLE_VALUE`, else the association of `handle` with `existingPort` under `completionKey`.
fn iocp_create(_c: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = long_at(args, 0);
    let existing = long_at(args, 1);
    let key = int_arg(args, 2);
    if handle == INVALID_HANDLE_VALUE {
        let id = next_port();
        ports().lock().insert(
            id,
            Arc::new(Port {
                queue: Mutex::new(VecDeque::new()),
                ready: Condvar::new(),
                closed: AtomicBool::new(false),
            }),
        );
        return Ok(Some(Value::Long(id)));
    }
    if port_of(existing).is_none() {
        return Err(ioex(
            "CreateIoCompletionPort failed: the completion port is closed",
        ));
    }
    associations().lock().insert(handle, (existing, key));
    Ok(Some(Value::Long(existing)))
}

fn iocp_close(_c: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = long_at(args, 0);
    if let Some(p) = ports().lock().remove(&handle) {
        p.closed.store(true, Ordering::Release);
        p.ready.notify_all();
    }
    associations().lock().retain(|_, (port, _)| *port != handle);
    Ok(None)
}

/// `getQueuedCompletionStatus(port, status)`: block for a completion, scatter a read's bytes into the Java
/// buffers, and fill `error` / `bytesTransferred` / `completionKey` / `overlapped` of `status`.
fn iocp_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = long_at(args, 0);
    let status = obj_arg(args, 1)?;
    let Some(port) = port_of(handle) else {
        return Err(ioex(
            "GetQueuedCompletionStatus failed: the completion port is closed",
        ));
    };
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    let taken = {
        let mut q = port.queue.lock();
        loop {
            if let Some(c) = q.pop_front() {
                break Some(c);
            }
            if port.closed.load(Ordering::Acquire) {
                break None;
            }
            port.ready.wait_for(&mut q, Duration::from_millis(500));
        }
    };
    ctx.end_blocking_region();
    let Some(c) = taken else {
        return Err(ioex(
            "GetQueuedCompletionStatus failed: the completion port is closed",
        ));
    };
    if let Some(rb) = c.read {
        let mut off = 0usize;
        for (addr, len) in rb.dests {
            if off >= rb.data.len() {
                break;
            }
            let n = len.min(rb.data.len() - off);
            if !ctx.copy_to_native_memory(addr, &rb.data[off..off + n]) {
                return Err(ioex(format!(
                    "GetQueuedCompletionStatus: invalid buffer address {addr:#x}"
                )));
            }
            off += n;
        }
    }
    ctx.set_field_by_name(status, "error", Value::Int(c.error));
    ctx.set_field_by_name(status, "bytesTransferred", Value::Int(c.bytes));
    ctx.set_field_by_name(status, "completionKey", Value::Int(c.key));
    ctx.set_field_by_name(status, "overlapped", Value::Long(c.overlapped));
    Ok(None)
}

/// `postQueuedCompletionStatus(port, key)`: a packet with no OVERLAPPED, the group's wakeup.
fn iocp_post(_c: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = long_at(args, 0);
    let key = int_arg(args, 1);
    if port_of(handle).is_none() {
        return Err(ioex(
            "PostQueuedCompletionStatus failed: the completion port is closed",
        ));
    }
    post(
        handle,
        Completion {
            key,
            overlapped: 0,
            error: 0,
            bytes: 0,
            read: None,
        },
    );
    Ok(None)
}

#[link(name = "Kernel32")]
extern "system" {
    fn FormatMessageW(
        flags: u32,
        source: *const core::ffi::c_void,
        message_id: u32,
        language_id: u32,
        buffer: *mut u16,
        size: u32,
        arguments: *const core::ffi::c_void,
    ) -> u32;
}

/// `getErrorMessage(error)`: `FormatMessageW(FORMAT_MESSAGE_FROM_SYSTEM)`, exactly as `Iocp.c` returns it
/// (the system text, trailing punctuation and all), or `null` when the system has none.
fn iocp_error_message(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x1000;
    let mut buf = [0u16; 255];
    // SAFETY: `buf` is a live, writable array of 255 UTF-16 units and the flags name no insert arguments.
    let len = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM,
            core::ptr::null(),
            int_arg(args, 0) as u32,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            core::ptr::null(),
        )
    } as usize;
    if len == 0 {
        return Ok(Some(Value::Object(None)));
    }
    // HotSpot's text carries neither FormatMessage's trailing CR LF nor its closing period (measured:
    // `WinAsync refused`, 1225 reads "... сетевое подключение").
    let text = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
    let text = text.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    let s = ctx.create_string_uninterned(text);
    Ok(Some(Value::Object(Some(s))))
}

// ---------------------------------------------------------------------------------------------
// WindowsAsynchronousSocketChannelImpl / WindowsAsynchronousServerSocketChannelImpl
// ---------------------------------------------------------------------------------------------

const SIZEOF_WSABUF: usize = 16;

/// The `(address, length)` of each of `count` `WSABUF { u32 len; u64 buf }` (offset 8) at `address`.
fn wsabufs(
    ctx: &mut dyn NativeContext,
    address: i64,
    count: i32,
) -> Result<Vec<(i64, usize)>, MethodCallFailed> {
    let count = count.clamp(0, 64) as usize;
    let mut raw = vec![0u8; count * SIZEOF_WSABUF];
    if !ctx.copy_from_native_memory(address, &mut raw) {
        return Err(ioex(format!("invalid WSABUF array address {address:#x}")));
    }
    Ok((0..count)
        .map(|i| {
            let b = &raw[i * SIZEOF_WSABUF..(i + 1) * SIZEOF_WSABUF];
            let len = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
            let ptr = i64::from_le_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]);
            (ptr, len)
        })
        .collect())
}

fn stream_of(id: i64) -> Option<Arc<TcpStream>> {
    match net_sockets().read().get(&(id as i32)) {
        Some(NetSocketHandle::Stream(s)) => Some(Arc::clone(s)),
        _ => None,
    }
}

/// `read0(socket, count, address, overlapped)`.
fn read0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = long_at(args, 0);
    let dests = wsabufs(ctx, long_at(args, 2), int_arg(args, 1))?;
    let overlapped = long_at(args, 3);
    let Some(stream) = stream_of(handle) else {
        return Err(ioex("WSARecv failed: the socket is not connected"));
    };
    let total: usize = dests.iter().map(|d| d.1).sum();
    spawn(move || {
        let mut buf = vec![0u8; total.max(1)];
        match (&*stream).read(&mut buf) {
            Ok(n) if !is_aborted(handle) => {
                buf.truncate(n);
                complete(
                    handle,
                    overlapped,
                    0,
                    n as i32,
                    Some(ReadBack { data: buf, dests }),
                );
            }
            Ok(_) => complete(handle, overlapped, ERROR_OPERATION_ABORTED, 0, None),
            Err(e) => complete(handle, overlapped, error_code(&e, handle), 0, None),
        }
    });
    Ok(Some(Value::Int(IOS_UNAVAILABLE)))
}

/// `write0(socket, count, address, overlapped)`: the bytes are copied now, the send runs in the worker.
fn write0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = long_at(args, 0);
    let bufs = wsabufs(ctx, long_at(args, 2), int_arg(args, 1))?;
    let overlapped = long_at(args, 3);
    let mut data = Vec::new();
    for (addr, len) in bufs {
        let at = data.len();
        data.resize(at + len, 0);
        if !ctx.copy_from_native_memory(addr, &mut data[at..]) {
            return Err(ioex(format!("WSASend: invalid buffer address {addr:#x}")));
        }
    }
    let Some(stream) = stream_of(handle) else {
        return Err(ioex("WSASend failed: the socket is not connected"));
    };
    spawn(move || {
        let n = data.len();
        match (&*stream).write_all(&data) {
            Ok(()) if !is_aborted(handle) => complete(handle, overlapped, 0, n as i32, None),
            Ok(()) => complete(handle, overlapped, ERROR_OPERATION_ABORTED, 0, None),
            Err(e) => complete(handle, overlapped, error_code(&e, handle), 0, None),
        }
    });
    Ok(Some(Value::Int(IOS_UNAVAILABLE)))
}

/// `connect0(socket, preferIPv6, remote, port, overlapped)`.
fn connect0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = long_at(args, 0);
    let remote = match args.get(2) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let port = int_arg(args, 3);
    let overlapped = long_at(args, 4);
    let target = join_host_port(&read_inet_address_text(ctx, remote), port);
    spawn(move || {
        if is_aborted(handle) {
            complete(handle, overlapped, ERROR_OPERATION_ABORTED, 0, None);
            return;
        }
        match crate::outbound_policy::policy_connect(&target) {
            Ok(stream) => {
                if is_aborted(handle) {
                    complete(handle, overlapped, ERROR_OPERATION_ABORTED, 0, None);
                    return;
                }
                net_sockets()
                    .write()
                    .insert(handle as i32, NetSocketHandle::Stream(Arc::new(stream)));
                net_apply_recorded_options(handle as i32);
                complete(handle, overlapped, 0, 0, None);
            }
            Err(crate::outbound_policy::PolicyConnectError::Io(e)) => {
                complete(handle, overlapped, error_code(&e, handle), 0, None)
            }
            Err(_) => complete(handle, overlapped, WSAEACCES, 0, None),
        }
    });
    Ok(Some(Value::Int(IOS_UNAVAILABLE)))
}

/// `accept0(listenSocket, acceptSocket, overlapped, dataBuffer)`: the accepted connection replaces the
/// pre-created socket `acceptSocket` in the registry, which is what `AcceptEx` does to its socket.
fn accept0(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let listen = long_at(args, 0);
    let accept = long_at(args, 1);
    let overlapped = long_at(args, 2);
    let listener = match net_sockets().read().get(&(listen as i32)) {
        Some(NetSocketHandle::Listener(l)) => Arc::clone(l),
        _ => return Err(ioex("AcceptEx failed: the socket is not listening")),
    };
    spawn(move || loop {
        if is_aborted(listen) || is_aborted(accept) {
            complete(listen, overlapped, ERROR_OPERATION_ABORTED, 0, None);
            return;
        }
        // Poll in slices so a `closesocket0` of the listener is noticed; the accept itself then does not block.
        let ready = {
            let Some(os) = nio_real::nio_os_fd(listen as i32) else {
                complete(listen, overlapped, ERROR_OPERATION_ABORTED, 0, None);
                return;
            };
            wepoll_readable(os.raw, 50)
        };
        if !ready {
            continue;
        }
        let accepted = listener.lock().accept();
        match accepted {
            Ok((stream, _)) => {
                if is_aborted(accept) {
                    complete(listen, overlapped, ERROR_OPERATION_ABORTED, 0, None);
                    return;
                }
                let _ = stream.set_nonblocking(false);
                net_sockets()
                    .write()
                    .insert(accept as i32, NetSocketHandle::Stream(Arc::new(stream)));
                net_apply_recorded_options(accept as i32);
                complete(listen, overlapped, 0, 0, None);
                return;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                complete(listen, overlapped, error_code(&e, listen), 0, None);
                return;
            }
        }
    });
    Ok(Some(Value::Int(IOS_UNAVAILABLE)))
}

#[repr(C)]
struct PollFd {
    fd: usize,
    events: i16,
    revents: i16,
}

#[link(name = "Ws2_32")]
extern "system" {
    fn WSAPoll(fds: *mut PollFd, nfds: u32, timeout: i32) -> i32;
}

/// Is `raw` readable (a connection waiting, for a listener) within `ms`?
fn wepoll_readable(raw: usize, ms: i32) -> bool {
    const POLLRDNORM: i16 = 0x0100;
    let mut fd = PollFd {
        fd: raw,
        events: POLLRDNORM,
        revents: 0,
    };
    // SAFETY: one live `PollFd` for the call.
    let rc = unsafe { WSAPoll(&mut fd, 1, ms) };
    rc > 0 && fd.revents != 0
}

fn noop(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// `shutdown0(socket, how)`: `SD_RECEIVE` / `SD_SEND` / `SD_BOTH`.
fn shutdown0(_c: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let how = match int_arg(args, 1) {
        0 => std::net::Shutdown::Read,
        1 => std::net::Shutdown::Write,
        _ => std::net::Shutdown::Both,
    };
    if let Some(s) = stream_of(long_at(args, 0)) {
        let _ = s.shutdown(how);
    }
    Ok(None)
}

/// `closesocket0(socket)`: every pending operation completes as aborted.
fn closesocket0(_c: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = long_at(args, 0);
    aborted().lock().insert(handle);
    if let Some(s) = stream_of(handle) {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
    close_net_fd(handle as i32);
    associations().lock().remove(&handle);
    Ok(None)
}

// ---------------------------------------------------------------------------------------------
// WindowsAsynchronousFileChannelImpl
// ---------------------------------------------------------------------------------------------

fn file_of(ctx: &mut dyn NativeContext, handle: i64) -> Result<std::fs::File, MethodCallFailed> {
    ctx.fd_table().clone_file(handle as _).map_err(|e| {
        crate::libnio_error::io_exception_with_last_error(&e, "the file channel is not open")
    })
}

/// `readFile(handle, address, len, position, overlapped)`.
fn read_file(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::windows::fs::FileExt;
    let handle = long_at(args, 0);
    let address = long_at(args, 1);
    let len = int_arg(args, 2).max(0) as usize;
    let position = long_at(args, 3).max(0) as u64;
    let overlapped = long_at(args, 4);
    let file = file_of(ctx, handle)?;
    spawn(move || {
        let mut buf = vec![0u8; len.max(1)];
        match file.seek_read(&mut buf[..len], position) {
            Ok(0) if len > 0 => complete(handle, overlapped, ERROR_HANDLE_EOF, 0, None),
            Ok(n) => {
                buf.truncate(n);
                complete(
                    handle,
                    overlapped,
                    0,
                    n as i32,
                    Some(ReadBack {
                        data: buf,
                        dests: vec![(address, len)],
                    }),
                );
            }
            Err(e) => complete(handle, overlapped, e.raw_os_error().unwrap_or(1), 0, None),
        }
    });
    Ok(Some(Value::Int(IOS_UNAVAILABLE)))
}

/// `writeFile(handle, address, len, position, overlapped)`.
fn write_file(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::windows::fs::FileExt;
    let handle = long_at(args, 0);
    let address = long_at(args, 1);
    let len = int_arg(args, 2).max(0) as usize;
    let position = long_at(args, 3).max(0) as u64;
    let overlapped = long_at(args, 4);
    let mut data = vec![0u8; len];
    if !ctx.copy_from_native_memory(address, &mut data) {
        return Err(ioex(format!(
            "WriteFile: invalid buffer address {address:#x}"
        )));
    }
    let file = file_of(ctx, handle)?;
    spawn(move || {
        let mut done = 0usize;
        while done < data.len() {
            match file.seek_write(&data[done..], position + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) => {
                    complete(handle, overlapped, e.raw_os_error().unwrap_or(1), 0, None);
                    return;
                }
            }
        }
        complete(handle, overlapped, 0, done as i32, None);
    });
    Ok(Some(Value::Int(IOS_UNAVAILABLE)))
}

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut core::ffi::c_void,
}

#[link(name = "kernel32")]
extern "system" {
    fn LockFileEx(
        file: *mut core::ffi::c_void,
        flags: u32,
        reserved: u32,
        low: u32,
        high: u32,
        overlapped: *mut Overlapped,
    ) -> i32;
}

/// `lockFile(handle, position, size, shared, overlapped)`: `LockFileEx` in the worker (it waits for the
/// lock on a non-overlapped handle), completing when the range is held.
fn lock_file(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::windows::io::AsRawHandle;
    let handle = long_at(args, 0);
    let position = long_at(args, 1).max(0) as u64;
    let size = long_at(args, 2).max(0) as u64;
    let shared = bool_arg(args, 3);
    let overlapped = long_at(args, 4);
    let file = file_of(ctx, handle)?;
    spawn(move || {
        const LOCKFILE_EXCLUSIVE_LOCK: u32 = 2;
        let mut ov = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: position as u32,
            offset_high: (position >> 32) as u32,
            event: core::ptr::null_mut(),
        };
        // SAFETY: `file` keeps the handle open for the call and `ov` is a live OVERLAPPED.
        let ok = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                if shared { 0 } else { LOCKFILE_EXCLUSIVE_LOCK },
                0,
                size as u32,
                (size >> 32) as u32,
                &mut ov,
            )
        };
        if ok != 0 {
            complete(handle, overlapped, 0, 0, None);
        } else {
            let e = std::io::Error::last_os_error();
            complete(handle, overlapped, e.raw_os_error().unwrap_or(1), 0, None);
        }
        drop(file);
    });
    Ok(Some(Value::Int(IOS_UNAVAILABLE)))
}

pub(super) fn register(r: &mut NativeMethodRegistry) {
    use cratonvm_native_api::NativeKind;
    let b = NativeKind::Bridge;
    let iocp = "sun/nio/ch/Iocp";
    r.register_with_kind(iocp, "initIDs", "()V", iocp_init_ids, b);
    r.register_with_kind(iocp, "createIoCompletionPort", "(JJII)J", iocp_create, b);
    r.register_with_kind(iocp, "close0", "(J)V", iocp_close, b);
    r.register_with_kind(
        iocp,
        "getQueuedCompletionStatus",
        "(JLsun/nio/ch/Iocp$CompletionStatus;)V",
        iocp_get,
        b,
    );
    r.register_with_kind(iocp, "postQueuedCompletionStatus", "(JI)V", iocp_post, b);
    r.register_with_kind(
        iocp,
        "getErrorMessage",
        "(I)Ljava/lang/String;",
        iocp_error_message,
        b,
    );

    let asc = "sun/nio/ch/WindowsAsynchronousSocketChannelImpl";
    r.register_with_kind(asc, "initIDs", "()V", iocp_init_ids, b);
    r.register_with_kind(
        asc,
        "connect0",
        "(JZLjava/net/InetAddress;IJ)I",
        connect0,
        b,
    );
    r.register_with_kind(asc, "updateConnectContext", "(J)V", noop, b);
    r.register_with_kind(asc, "read0", "(JIJJ)I", read0, b);
    r.register_with_kind(asc, "write0", "(JIJJ)I", write0, b);
    r.register_with_kind(asc, "shutdown0", "(JI)V", shutdown0, b);
    r.register_with_kind(asc, "closesocket0", "(J)V", closesocket0, b);

    let assc = "sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl";
    r.register_with_kind(assc, "initIDs", "()V", iocp_init_ids, b);
    r.register_with_kind(assc, "accept0", "(JJJJ)I", accept0, b);
    r.register_with_kind(assc, "updateAcceptContext", "(JJ)V", noop, b);
    r.register_with_kind(assc, "closesocket0", "(J)V", closesocket0, b);

    let afc = "sun/nio/ch/WindowsAsynchronousFileChannelImpl";
    r.register_with_kind(afc, "readFile", "(JJIJJ)I", read_file, b);
    r.register_with_kind(afc, "writeFile", "(JJIJJ)I", write_file, b);
    r.register_with_kind(afc, "lockFile", "(JJJZJ)I", lock_file, b);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockNativeContext;

    #[test]
    fn a_port_hands_completions_back_in_order_and_drops_them_once_closed() {
        let mut ctx = MockNativeContext::new();
        let port = match iocp_create(
            &mut ctx,
            &[
                Value::Long(-1),
                Value::Long(0),
                Value::Int(0),
                Value::Int(1),
            ],
        )
        .unwrap()
        {
            Some(Value::Long(p)) => p,
            other => panic!("{other:?}"),
        };
        iocp_create(
            &mut ctx,
            &[
                Value::Long(77),
                Value::Long(port),
                Value::Int(5),
                Value::Int(0),
            ],
        )
        .unwrap();
        complete(77, 0x1000, 0, 4, None);
        complete(77, 0x2000, 995, 0, None);
        let p = port_of(port).unwrap();
        let mut q = p.queue.lock();
        let a = q.pop_front().unwrap();
        let b = q.pop_front().unwrap();
        assert_eq!((a.key, a.overlapped, a.error, a.bytes), (5, 0x1000, 0, 4));
        assert_eq!((b.key, b.overlapped, b.error), (5, 0x2000, 995));
        assert!(q.is_empty());
        drop(q);
        iocp_close(&mut ctx, &[Value::Long(port)]).unwrap();
        complete(77, 0x3000, 0, 0, None);
        assert!(port_of(port).is_none());
    }

    #[test]
    fn wsabufs_read_the_len_and_pointer_of_each_entry() {
        let raw = [0u8; 16];
        let len = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        assert_eq!(len, 0);
        assert_eq!(SIZEOF_WSABUF, 16);
    }

    #[test]
    fn an_aborted_socket_reports_operation_aborted_whatever_the_error() {
        aborted().lock().insert(424_242);
        let e = std::io::Error::from_raw_os_error(WSAECONNRESET);
        assert_eq!(error_code(&e, 424_242), ERROR_OPERATION_ABORTED);
        assert_eq!(error_code(&e, 1), 64);
        assert_eq!(overlapped_error(WSAECONNREFUSED), 1225);
    }
}
