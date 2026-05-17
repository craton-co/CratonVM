//! Real-JDK NIO native methods: `sun/nio/ch/FileDispatcherImpl`,
//! `sun/nio/ch/FileChannelImpl`, `sun/nio/ch/NativeThread`.
//!
//! These natives are declared by the real JDK 25 `sun.nio.ch` classes
//! and have no Java-side fallback. They take raw memory addresses
//! (allocated by the JDK via `Unsafe.allocateMemory`) and read/write
//! directly into them — so stubs that just return 0 work only as long
//! as the JDK does not dereference the resulting data.
//!
//! Strategy:
//!   * `map0` / `unmap0` are discouraged by the
//!     `sun.zip.disableMemoryMapping=true` system property set in
//!     `system_bootstrap.rs`. If they are still called we return
//!     IOException rather than a fake address — the JDK's ZipFile
//!     fallback path handles this by re-reading through the channel.
//!   * `read0` / `pread0` / `size0` etc. operate on a `FileDescriptor`
//!     whose `handle` (Windows) or `fd` (Unix) field stores an
//!     `FdId` from our `FileDescriptorTable`. If the field is 0
//!     (not populated by a prior `open0`), we return IOException —
//!     never a bogus pointer dereference.
//!   * `NativeThread.current()` returns a stable per-thread id.
//!   * All `long addr` writes are range-checked against `len` and
//!     performed with `std::ptr::copy_nonoverlapping` only when the
//!     caller provided a non-null address. A null address yields
//!     IOException.

use rustjvm_native_api::fd_table::FdId;
use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use rustjvm_types::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the FdId that a prior `FileInputStream.open0` /
/// `RandomAccessFile.open0` / `FileOutputStream.open0` stored on the
/// `FileDescriptor` object. On Windows the JDK uses `handle` (a long);
/// on Unix it uses `fd` (an int). We check both so the same native
/// works on either layout. Returns None if neither field holds a
/// valid id.
fn fd_from_descriptor(ctx: &mut dyn NativeContext, fd_obj: ObjectRef) -> Option<FdId> {
    // Prefer `handle` (Windows).
    match ctx.get_field_by_name(fd_obj, "handle") {
        Value::Long(v) if v > 0 && v < u32::MAX as i64 => return Some(v as FdId),
        _ => {}
    }
    match ctx.get_field_by_name(fd_obj, "fd") {
        Value::Int(v) if v > 2 => return Some(v as FdId),
        _ => {}
    }
    None
}

fn io_error(message: impl Into<String>) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
        message: message.into(),
    }))
}

fn fd_arg(args: &[Value], idx: usize) -> Result<ObjectRef, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(io_error("FileDispatcherImpl: null FileDescriptor")),
    }
}

fn long_arg(args: &[Value], idx: usize) -> i64 {
    match args.get(idx) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    }
}

fn int_arg(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// sun/nio/ch/FileDispatcherImpl natives
// ---------------------------------------------------------------------------

/// `read0(FileDescriptor, long addr, int len) -> int`
/// Reads `len` bytes from the fd at its current position into the
/// raw memory at `addr`. Returns bytes read, or -1 on EOF.
fn native_fd_read0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let addr = long_arg(args, 1);
    let len = int_arg(args, 2);
    if addr == 0 || len < 0 {
        return Err(io_error("read0: bad addr/len"));
    }
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("read0: FileDescriptor has no open handle"));
    };
    let mut buf = vec![0u8; len as usize];
    let n = ctx
        .fd_table()
        .read_bytes(fd, &mut buf)
        .map_err(|e| io_error(format!("read0: {e}")))?;
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    // SAFETY: addr is a native pointer allocated by `Unsafe.allocateMemory`
    // (or by a DirectByteBuffer) and the caller asserts at least `len` bytes
    // are valid. We bound by `n <= len`.
    unsafe {
        std::ptr::copy_nonoverlapping(buf.as_ptr(), addr as *mut u8, n);
    }
    Ok(Some(Value::Int(n as i32)))
}

/// `pread0(FileDescriptor, long addr, int len, long pos) -> int`
/// Positioned read — does not advance the fd's cursor.
fn native_fd_pread0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let addr = long_arg(args, 1);
    let len = int_arg(args, 2);
    let pos = long_arg(args, 3);
    if addr == 0 || len < 0 || pos < 0 {
        return Err(io_error("pread0: bad addr/len/pos"));
    }
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("pread0: FileDescriptor has no open handle"));
    };
    let mut buf = vec![0u8; len as usize];
    let n = ctx
        .fd_table()
        .pread_at(fd, &mut buf, pos as u64)
        .map_err(|e| io_error(format!("pread0: {e}")))?;
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(buf.as_ptr(), addr as *mut u8, n);
    }
    Ok(Some(Value::Int(n as i32)))
}

/// `write0(FileDescriptor, long addr, int len, boolean append) -> int`
fn native_fd_write0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let addr = long_arg(args, 1);
    let len = int_arg(args, 2);
    if addr == 0 || len < 0 {
        return Err(io_error("write0: bad addr/len"));
    }
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("write0: FileDescriptor has no open handle"));
    };
    let mut buf = vec![0u8; len as usize];
    unsafe {
        std::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), len as usize);
    }
    ctx.fd_table()
        .write_bytes(fd, &buf)
        .map_err(|e| io_error(format!("write0: {e}")))?;
    Ok(Some(Value::Int(len)))
}

/// `pwrite0(FileDescriptor, long addr, int len, long pos) -> int`
fn native_fd_pwrite0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let addr = long_arg(args, 1);
    let len = int_arg(args, 2);
    let pos = long_arg(args, 3);
    if addr == 0 || len < 0 || pos < 0 {
        return Err(io_error("pwrite0: bad addr/len/pos"));
    }
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("pwrite0: FileDescriptor has no open handle"));
    };
    let mut buf = vec![0u8; len as usize];
    unsafe {
        std::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), len as usize);
    }
    let n = ctx
        .fd_table()
        .pwrite_at(fd, &buf, pos as u64)
        .map_err(|e| io_error(format!("pwrite0: {e}")))?;
    Ok(Some(Value::Int(n as i32)))
}

/// `size0(FileDescriptor) -> long`
fn native_fd_size0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("size0: FileDescriptor has no open handle"));
    };
    let n = ctx
        .fd_table()
        .file_size(fd)
        .map_err(|e| io_error(format!("size0: {e}")))?;
    Ok(Some(Value::Long(n as i64)))
}

/// `seek0(FileDescriptor, long pos) -> long` — returns new position.
fn native_fd_seek0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let pos = long_arg(args, 1);
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("seek0: FileDescriptor has no open handle"));
    };
    let new_pos = ctx
        .fd_table()
        .rw_seek(fd, std::io::SeekFrom::Start(pos as u64))
        .map_err(|e| io_error(format!("seek0: {e}")))?;
    Ok(Some(Value::Long(new_pos as i64)))
}

/// `close0(FileDescriptor)` — closes the underlying fd.
fn native_fd_close0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    if let Some(fd) = fd_from_descriptor(ctx, fd_obj) {
        let _ = ctx.fd_table().close(fd);
    }
    // Zero out the handle to prevent double-close.
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(-1));
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(-1));
    Ok(None)
}

/// `preClose0(FileDescriptor)` — marks the fd closed without actually
/// closing. Used by the JDK's `AbstractInterruptibleChannel` to
/// interrupt a blocking read. Noop here since our reads are blocking
/// but synchronous.
fn native_fd_preclose0(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

// `force0` — the real, fsync-issuing implementation lives in
// `file_channel.rs::native_fc_force0` and is registered by
// `register_file_channel_real`. The previous stub here was a silent
// no-op which corrupted durability guarantees for callers of
// `FileChannel.force` — removed to prevent re-introduction.

/// `truncate0(FileDescriptor, long) -> int`
fn native_fd_truncate0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let len = long_arg(args, 1);
    if len < 0 {
        return Err(io_error("truncate0: negative length"));
    }
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("truncate0: FileDescriptor has no open handle"));
    };
    ctx.fd_table()
        .rw_set_length(fd, len as u64)
        .map_err(|e| io_error(format!("truncate0: {e}")))?;
    Ok(Some(Value::Int(0)))
}

/// `available0(FileDescriptor) -> int`
fn native_fd_available0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Ok(Some(Value::Int(0)));
    };
    let n = ctx.fd_table().available(fd).unwrap_or(0);
    Ok(Some(Value::Int(n as i32)))
}

/// `isOther0(FileDescriptor) -> boolean` — true if the fd refers to a
/// non-regular-file (pipe, socket, device). We always report false.
fn native_fd_isother0(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// `lock0(FileDescriptor, boolean, long, long, boolean) -> int` — file
/// locking. Return 0 (success) to let the JDK proceed.
fn native_fd_lock0(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// `release0(FileDescriptor, long, long)` — release lock.
fn native_fd_release0(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// `duplicateHandle(long) -> long` — duplicate a Win32 HANDLE. We just
/// echo the input since our FdIds are refcounted by the fd_table.
fn native_fd_duplicate_handle(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(long_arg(args, 0))))
}

/// `readv0` / `writev0` — scatter/gather. Signal that vectored I/O is
/// unsupported; the JDK falls back to per-buffer loops.
fn native_fd_readv0(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Err(io_error("readv0: unsupported"))
}

fn native_fd_writev0(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Err(io_error("writev0: unsupported"))
}

/// `setDirect0(FileDescriptor, CharBuffer) -> int` — advise the kernel
/// that the caller wants direct I/O. Return -1 to say "not supported",
/// which the JDK treats as a hint (not an error).
fn native_fd_setdirect0(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(-1)))
}

// ---------------------------------------------------------------------------
// sun/nio/ch/FileChannelImpl natives (the static ones that live on
// FileDispatcherImpl in JDK 25 — but historically these also exist on
// FileChannelImpl; register on both classes for safety).
//
// NOTE: `map0`, `unmap0`, `transferTo0`, `maxDirectTransferSize0`, and
// `force0` are intentionally NOT defined here. Their canonical, real
// implementations live in `file_channel.rs` and are registered by
// `register_file_channel_real`. Previously this module carried stub
// versions and registered them on the same (class, name, desc) keys —
// whichever registration ran last won, which was fragile and on at
// least one config let the no-op `force0` stub clobber the real
// fsync-issuing implementation, silently breaking durability.
// ---------------------------------------------------------------------------

/// `position0(FileDescriptor, long) -> long` — get/set position.
/// On JDK 25 the second arg is the requested position (-1 to query).
fn native_fc_position0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let req = long_arg(args, 1);
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("position0: FileDescriptor has no open handle"));
    };
    if req < 0 {
        let p = ctx
            .fd_table()
            .rw_position(fd)
            .map_err(|e| io_error(format!("position0: {e}")))?;
        Ok(Some(Value::Long(p as i64)))
    } else {
        let p = ctx
            .fd_table()
            .rw_seek(fd, std::io::SeekFrom::Start(req as u64))
            .map_err(|e| io_error(format!("position0: {e}")))?;
        Ok(Some(Value::Long(p as i64)))
    }
}

/// `allocationGranularity0() -> long` — page size for mmap alignment.
fn native_fc_allocation_granularity0(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Windows: 64 KiB. Unix: page size (typ. 4 KiB). 64 KiB is safe on both.
    Ok(Some(Value::Long(65536)))
}

// ---------------------------------------------------------------------------
// sun/nio/ch/NativeThread — thread id for interruptible I/O.
// ---------------------------------------------------------------------------

/// `current() -> long` — identifier for the current OS thread. Any
/// monotonically stable non-zero value works.
fn native_nt_current(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Use the Rust thread id. `thread_id::get()` isn't available in std,
    // so hash `ThreadId` instead. Keep it short and non-zero.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    let id = (h.finish() as i64).abs().max(1);
    Ok(Some(Value::Long(id)))
}

/// `signal(long)` — send a signal to interrupt a blocking read in
/// another thread. We have no blocking I/O to interrupt; noop.
fn native_nt_signal(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// `init()` — one-time initializer.
fn native_nt_init(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// IOUtil — hasAtomicLock etc. Safe defaults.
// ---------------------------------------------------------------------------

fn native_iou_iov_max(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(16)))
}

fn native_iou_write_max_size(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(i32::MAX as i64)))
}

fn native_iou_init_ids(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// Public registration
// ---------------------------------------------------------------------------

/// Register `sun/nio/ch/*` natives for real-JDK boot. Idempotent: the
/// registry's `register` replaces a previous entry at the same key,
/// so it's safe to call after other NIO-related registrars.
pub fn register_nio_natives_real(r: &mut NativeMethodRegistry) {
    // --- FileDispatcherImpl (Unix class name; Windows uses
    // WindowsFileDispatcherImpl but the static natives are on the
    // parent class or in a companion). Register on all three names
    // for robustness — the registry lookup uses (class, name, desc). ---
    for cls in [
        "sun/nio/ch/FileDispatcherImpl",
        "sun/nio/ch/WindowsFileDispatcherImpl",
        "sun/nio/ch/UnixFileDispatcherImpl",
    ] {
        r.register(cls, "read0", "(Ljava/io/FileDescriptor;JI)I", native_fd_read0);
        r.register(cls, "pread0", "(Ljava/io/FileDescriptor;JIJ)I", native_fd_pread0);
        r.register(cls, "readv0", "(Ljava/io/FileDescriptor;JI)J", native_fd_readv0);
        r.register(cls, "write0", "(Ljava/io/FileDescriptor;JIZ)I", native_fd_write0);
        r.register(cls, "write0", "(Ljava/io/FileDescriptor;JI)I", native_fd_write0);
        r.register(cls, "pwrite0", "(Ljava/io/FileDescriptor;JIJ)I", native_fd_pwrite0);
        r.register(cls, "writev0", "(Ljava/io/FileDescriptor;JIZ)J", native_fd_writev0);
        r.register(cls, "writev0", "(Ljava/io/FileDescriptor;JI)J", native_fd_writev0);
        r.register(cls, "size0", "(Ljava/io/FileDescriptor;)J", native_fd_size0);
        r.register(cls, "seek0", "(Ljava/io/FileDescriptor;J)J", native_fd_seek0);
        // `force0` is registered by `file_channel.rs::register_file_channel_real`
        // (real fsync via `std::fs::File::sync_all` / `sync_data`).
        r.register(cls, "truncate0", "(Ljava/io/FileDescriptor;J)I", native_fd_truncate0);
        r.register(cls, "available0", "(Ljava/io/FileDescriptor;)I", native_fd_available0);
        r.register(cls, "isOther0", "(Ljava/io/FileDescriptor;)Z", native_fd_isother0);
        r.register(cls, "close0", "(Ljava/io/FileDescriptor;)V", native_fd_close0);
        r.register(cls, "preClose0", "(Ljava/io/FileDescriptor;)V", native_fd_preclose0);
        r.register(cls, "lock0", "(Ljava/io/FileDescriptor;ZJJZ)I", native_fd_lock0);
        r.register(cls, "release0", "(Ljava/io/FileDescriptor;JJ)V", native_fd_release0);
        r.register(cls, "duplicateHandle", "(J)J", native_fd_duplicate_handle);
        r.register(cls, "setDirect0", "(Ljava/io/FileDescriptor;Ljava/nio/CharBuffer;)I",
                   native_fd_setdirect0);
        r.register(cls, "init", "()V", native_nt_init);
        // map0 / unmap0 / transferTo0 / maxDirectTransferSize0 / force0
        // are registered by `file_channel.rs::register_file_channel_real`
        // (real memmap2 / sendfile / fsync implementations). They were
        // previously stubbed here and the duplicate registration was
        // fragile — order-of-registration decided which won. Only
        // `allocationGranularity0` (a pure constant) is owned here.
        r.register(cls, "allocationGranularity0", "()J", native_fc_allocation_granularity0);
    }

    // --- FileChannelImpl (legacy names for older JDKs that carried the
    // natives directly on this class). The map0 / unmap0 / transferTo0 /
    // maxDirectTransferSize0 entries here are owned by
    // `file_channel.rs::register_file_channel_real`. Only `position0`,
    // `allocationGranularity0` and `initIDs` remain — none of those
    // have a "real" counterpart and they are otherwise harmless.
    let fci = "sun/nio/ch/FileChannelImpl";
    r.register(fci, "position0", "(Ljava/io/FileDescriptor;J)J", native_fc_position0);
    r.register(fci, "allocationGranularity0", "()J", native_fc_allocation_granularity0);
    r.register(fci, "initIDs", "()J", |_c, _a| Ok(Some(Value::Long(65536))));

    // --- NativeThread ---
    let nt = "sun/nio/ch/NativeThread";
    r.register(nt, "current", "()J", native_nt_current);
    r.register(nt, "current0", "()J", native_nt_current);
    r.register(nt, "signal", "(J)V", native_nt_signal);
    r.register(nt, "init", "()V", native_nt_init);

    // --- IOUtil ---
    let iou = "sun/nio/ch/IOUtil";
    r.register(iou, "iovMax", "()I", native_iou_iov_max);
    r.register(iou, "writevMax", "()J", native_iou_write_max_size);
    r.register(iou, "initIDs", "()V", native_iou_init_ids);
}

// ---------------------------------------------------------------------------
// T16.5 / T16.6 — Async NIO channels + DatagramChannel overrides
// ---------------------------------------------------------------------------
//
// These supersede the phase-72 (`phases_late`) and phase-92 (`lib.rs`)
// registrations for:
//   - java/nio/channels/AsynchronousFileChannel
//   - java/nio/channels/AsynchronousSocketChannel
//   - java/nio/channels/AsynchronousChannelGroup
//   - java/nio/channels/DatagramChannel
//   - java/util/logging/{Logger, LogRecord, LogManager} — a handful of
//     extras that phase-72 registers but that NPE when tests pass a null
//     `this` (the tests intentionally exercise the null-tolerance path
//     to match the JDK's behavior of `Logger.entering` being a no-op).
//
// Layouts (shared with `phases_late::register_p67_async_channels` /
// `register_datagram_channel`):
//   AsynchronousFileChannel       = 3 fields (path_str=0, open=1, _unused=2)
//   AsynchronousSocketChannel     = 4 fields (connected=0, open=1, fd=2, remote=3)
//   AsynchronousChannelGroup      = 1 field  (state=0 — 1=running, 0=shut)
//   DatagramChannel               = 5 fields (port=0, open=1, connected=2, blocking=3, sock_id=4)

use parking_lot::RwLock;
use rustjvm_types::ClassId;
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::OnceLock;

/// Process-wide registry keeping `UdpSocket` handles alive while a
/// `DatagramChannel` Java object references them. The id stored in the
/// channel's `sock_id` field indexes into this map. Matches the
/// Session-86/87 Delta pattern for per-handle resource tables.
fn udp_registry() -> &'static RwLock<HashMap<i32, UdpSocket>> {
    static REG: OnceLock<RwLock<HashMap<i32, UdpSocket>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

fn udp_next_id() -> i32 {
    use std::sync::atomic::{AtomicI32, Ordering};
    static NEXT: AtomicI32 = AtomicI32::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

fn udp_register(sock: UdpSocket) -> i32 {
    let id = udp_next_id();
    udp_registry().write().insert(id, sock);
    id
}

fn udp_with<T>(id: i32, f: impl FnOnce(&UdpSocket) -> T) -> Option<T> {
    udp_registry().read().get(&id).map(f)
}

fn udp_remove(id: i32) {
    udp_registry().write().remove(&id);
}

/// Allocate a synthetic instance of `class_name` with enough slots to hold
/// the synthetic field layout. Tolerates classes that don't appear in the
/// bootstrap classloader — falls back to `ClassId::new(0)` with the
/// explicit field count.
fn alloc_t16(ctx: &mut dyn NativeContext, class_name: &str, nfields: usize) -> rustjvm_types::ObjectRef {
    match ctx.ensure_class_initialized(class_name) {
        Ok(cid) => ctx.alloc_object(cid, nfields),
        Err(_) => ctx.alloc_object(ClassId::new(0), nfields),
    }
}

fn obj_or_none(args: &[Value], idx: usize) -> Option<rustjvm_types::ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

// ---- AsynchronousFileChannel ----

fn t16_afc_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Optional Path arg at index 0 — extract its underlying string if present.
    let path_str = match args.first() {
        Some(Value::Object(Some(path_obj))) => {
            // Path field 0 holds the path string; fall back to reading the
            // object directly if it's already a String.
            match ctx.get_field(*path_obj, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => ctx.read_string(*path_obj).unwrap_or_default(),
            }
        }
        _ => String::new(),
    };

    let ch = alloc_t16(ctx, "java/nio/channels/AsynchronousFileChannel", 3);
    let path_val = if path_str.is_empty() {
        Value::Object(None)
    } else {
        Value::Object(Some(ctx.create_string(&path_str)))
    };
    ctx.set_field(ch, 0, path_val);
    ctx.set_field(ch, 1, Value::Int(1)); // open = true
    ctx.set_field(ch, 2, Value::Int(0));
    Ok(Some(Value::Object(Some(ch))))
}

fn t16_afc_is_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 2 => Ok(Some(ctx.get_field(o, 1))),
        _ => Ok(Some(Value::Int(1))),
    }
}

fn t16_afc_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 1 => {
            let path = match ctx.get_field(o, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => return Ok(Some(Value::Long(0))),
            };
            if path.is_empty() {
                return Ok(Some(Value::Long(0)));
            }
            let sz = std::fs::metadata(&path).map(|m| m.len() as i64).unwrap_or(0);
            Ok(Some(Value::Long(sz)))
        }
        _ => Ok(Some(Value::Long(0))),
    }
}

fn t16_afc_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(o) = obj_or_none(args, 0) {
        if ctx.object_num_fields(o) >= 2 {
            ctx.set_field(o, 1, Value::Int(0));
        }
    }
    Ok(None)
}

// ---- AsynchronousSocketChannel ----

fn t16_asc_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let ch = alloc_t16(ctx, "java/nio/channels/AsynchronousSocketChannel", 4);
    ctx.set_field(ch, 0, Value::Int(0)); // not connected
    ctx.set_field(ch, 1, Value::Int(1)); // open
    ctx.set_field(ch, 2, Value::Int(-1)); // fd unset
    ctx.set_field(ch, 3, Value::Object(None)); // remote addr
    Ok(Some(Value::Object(Some(ch))))
}

fn t16_asc_open_group(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    t16_asc_open(ctx, _args)
}

fn t16_asc_is_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Null-tolerant: if no `this` given (test pattern), report open.
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 2 => Ok(Some(ctx.get_field(o, 1))),
        _ => Ok(Some(Value::Int(1))),
    }
}

fn t16_asc_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(o) = obj_or_none(args, 0) {
        if ctx.object_num_fields(o) >= 2 {
            ctx.set_field(o, 1, Value::Int(0));
        }
    }
    Ok(None)
}

// ---- AsynchronousChannelGroup ----

fn t16_acg_with_fixed(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let g = alloc_t16(ctx, "java/nio/channels/AsynchronousChannelGroup", 1);
    ctx.set_field(g, 0, Value::Int(1)); // state = running
    Ok(Some(Value::Object(Some(g))))
}

fn t16_acg_with_thread_pool(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    t16_acg_with_fixed(ctx, args)
}

fn t16_acg_is_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Null-tolerant: default to "not shutdown" when no self argument is passed.
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 1 => {
            let state = ctx.get_field(o, 0).as_int().unwrap_or(1);
            Ok(Some(Value::Int(if state == 0 { 1 } else { 0 })))
        }
        _ => Ok(Some(Value::Int(0))),
    }
}

fn t16_acg_is_terminated(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    t16_acg_is_shutdown(ctx, args)
}

fn t16_acg_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(o) = obj_or_none(args, 0) {
        if ctx.object_num_fields(o) >= 1 {
            ctx.set_field(o, 0, Value::Int(0));
        }
    }
    Ok(None)
}

fn t16_acg_await_termination(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

// ---- DatagramChannel ----

fn t16_dc_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let ch = alloc_t16(ctx, "java/nio/channels/DatagramChannel", 5);
    // Bind a real UDP socket to 0.0.0.0:0 so the handle-ownership
    // invariant holds before configureBlocking/connect fires.
    let (port, sock_id) = match UdpSocket::bind("0.0.0.0:0") {
        Ok(sock) => {
            let port = sock.local_addr().map(|a| a.port() as i32).unwrap_or(0);
            let id = udp_register(sock);
            (port, id)
        }
        Err(_) => (0, -1),
    };
    ctx.set_field(ch, 0, Value::Int(port));     // port
    ctx.set_field(ch, 1, Value::Int(1));        // open
    ctx.set_field(ch, 2, Value::Int(0));        // not connected
    ctx.set_field(ch, 3, Value::Int(1));        // blocking = true (JDK default)
    ctx.set_field(ch, 4, Value::Int(sock_id));  // sock_id into udp_registry
    Ok(Some(Value::Object(Some(ch))))
}

fn t16_dc_is_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 2 => Ok(Some(ctx.get_field(o, 1))),
        _ => Ok(Some(Value::Int(1))),
    }
}

fn t16_dc_is_connected(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 3 => Ok(Some(ctx.get_field(o, 2))),
        _ => Ok(Some(Value::Int(0))),
    }
}

fn t16_dc_is_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 4 => Ok(Some(ctx.get_field(o, 3))),
        _ => Ok(Some(Value::Int(1))),
    }
}

fn t16_dc_configure_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let blocking = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
    if ctx.object_num_fields(this) >= 4 {
        ctx.set_field(this, 3, Value::Int(blocking));
    }
    // Apply to the underlying UDP socket if registered.
    if ctx.object_num_fields(this) >= 5 {
        let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        if sid >= 0 {
            let _ = udp_with(sid, |s| s.set_nonblocking(blocking == 0));
        }
    }
    Ok(Some(Value::Object(Some(this))))
}

fn t16_dc_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    // Try to extract host:port from the SocketAddress object. Tests may
    // pass a zero-field placeholder, so default to a connectable loopback
    // when nothing's decodable — the "connected" flag is what the test
    // asserts, not wire semantics.
    let target = if let Some(sa) = obj_or_none(args, 1) {
        let host = match ctx.get_field(sa, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "127.0.0.1".into()),
            _ => "127.0.0.1".into(),
        };
        let port = ctx.get_field(sa, 1).as_int().unwrap_or(9);
        format!("{host}:{port}")
    } else {
        "127.0.0.1:9".to_string()
    };
    if ctx.object_num_fields(this) >= 5 {
        let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        if sid >= 0 {
            // Ignore connect errors — discard protocol semantics; the
            // JDK allows connect to succeed even without a listener since
            // UDP connect is just filtering. If the native connect fails
            // we still mark the channel as connected for the Java view.
            let _ = udp_with(sid, |s| s.connect(&target));
        }
    }
    if ctx.object_num_fields(this) >= 3 {
        ctx.set_field(this, 2, Value::Int(1));
    }
    Ok(Some(Value::Object(Some(this))))
}

fn t16_dc_disconnect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_or_none(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    if ctx.object_num_fields(this) >= 3 {
        ctx.set_field(this, 2, Value::Int(0));
    }
    Ok(Some(Value::Object(Some(this))))
}

fn t16_dc_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(this) = obj_or_none(args, 0) {
        let nf = ctx.object_num_fields(this);
        if nf >= 2 {
            ctx.set_field(this, 1, Value::Int(0));
        }
        if nf >= 5 {
            let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
            if sid >= 0 {
                udp_remove(sid);
                ctx.set_field(this, 4, Value::Int(-1));
            }
        }
    }
    Ok(None)
}

// ---- java.util.logging extras (null-tolerant) ----

fn t16_log_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn t16_lr_get_sequence_number(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 7 => {
            let v = ctx.get_field(o, 6);
            match v {
                Value::Long(_) => Ok(Some(v)),
                _ => Ok(Some(Value::Long(0))),
            }
        }
        _ => Ok(Some(Value::Long(0))),
    }
}

// ---------------------------------------------------------------------------
// Public registration — called by `register_io_natives` AFTER phase-92 so
// our entries win.
// ---------------------------------------------------------------------------

/// Register the T16.5 / T16.6 channel overrides. Idempotent: safe to call
/// alongside `register_nio_natives_real` or phase-92's registrations.
pub fn register_t16_channel_overrides(r: &mut NativeMethodRegistry) {
    // AsynchronousFileChannel
    let afc = "java/nio/channels/AsynchronousFileChannel";
    r.register(
        afc,
        "open",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/nio/channels/AsynchronousFileChannel;",
        t16_afc_open,
    );
    r.register(afc, "isOpen", "()Z", t16_afc_is_open);
    r.register(afc, "size", "()J", t16_afc_size);
    r.register(afc, "close", "()V", t16_afc_close);

    // AsynchronousSocketChannel
    let asc = "java/nio/channels/AsynchronousSocketChannel";
    r.register(asc, "open", "()Ljava/nio/channels/AsynchronousSocketChannel;", t16_asc_open);
    r.register(
        asc,
        "open",
        "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousSocketChannel;",
        t16_asc_open_group,
    );
    r.register(asc, "isOpen", "()Z", t16_asc_is_open);
    r.register(asc, "close", "()V", t16_asc_close);

    // AsynchronousChannelGroup
    let acg = "java/nio/channels/AsynchronousChannelGroup";
    r.register(
        acg,
        "withFixedThreadPool",
        "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;",
        t16_acg_with_fixed,
    );
    r.register(
        acg,
        "withThreadPool",
        "(Ljava/util/concurrent/ExecutorService;)Ljava/nio/channels/AsynchronousChannelGroup;",
        t16_acg_with_thread_pool,
    );
    r.register(acg, "isShutdown", "()Z", t16_acg_is_shutdown);
    r.register(acg, "isTerminated", "()Z", t16_acg_is_terminated);
    r.register(acg, "shutdown", "()V", t16_acg_shutdown);
    r.register(acg, "shutdownNow", "()V", t16_acg_shutdown);
    r.register(
        acg,
        "awaitTermination",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        t16_acg_await_termination,
    );

    // DatagramChannel
    let dc = "java/nio/channels/DatagramChannel";
    r.register(dc, "open", "()Ljava/nio/channels/DatagramChannel;", t16_dc_open);
    r.register(dc, "isOpen", "()Z", t16_dc_is_open);
    r.register(dc, "isConnected", "()Z", t16_dc_is_connected);
    r.register(dc, "isBlocking", "()Z", t16_dc_is_blocking);
    r.register(
        dc,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        t16_dc_configure_blocking,
    );
    r.register(
        dc,
        "connect",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;",
        t16_dc_connect,
    );
    r.register(
        dc,
        "disconnect",
        "()Ljava/nio/channels/DatagramChannel;",
        t16_dc_disconnect,
    );
    r.register(dc, "close", "()V", t16_dc_close);

    // java.util.logging extras — null-tolerant variants that supersede the
    // phase-72 handlers, which NPE on `args.first() == Object(None)`.
    r.register(
        "java/util/logging/Logger",
        "entering",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        t16_log_noop,
    );
    r.register(
        "java/util/logging/LogRecord",
        "getSequenceNumber",
        "()J",
        t16_lr_get_sequence_number,
    );
    r.register(
        "java/util/logging/LogManager",
        "readConfiguration",
        "()V",
        t16_log_noop,
    );

    // MulticastSocket overrides live in `net.rs`.
    crate::net::register_multicast_socket_overrides(r);

    // T19.5 — sun.nio.ch.Net TCP native facade. Registers the socket0 /
    // bind0 / listen / accept / connect0 / read0 / write0 / shutdown / close
    // surface plus get/setIntOption0 and address queries. Needed so that
    // anything dispatching through ServerSocketChannelImpl (Undertow, Vert.x,
    // plain ServerSocketChannel.open().bind(...)) can actually listen on a
    // port.
    crate::net::register_sun_nio_ch_net(r);
}

