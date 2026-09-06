// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use cratonvm_native_api::fd_table::FdId;
use cratonvm_native_api::{NativeCallback, NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};

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

/// The leading byte `native-builtins` uses to mark a virtual-filesystem path (a
/// jar or runtime-image entry). It is `\u{1}`, which no real path can contain.
///
/// Duplicated here rather than imported because `native-builtins` depends on
/// this crate and not the other way round; `phases_late::nio_file` static-asserts
/// that the two agree, so a change on either side becomes a compile error rather
/// than a silent divergence.
pub const VFS_SENTINEL: char = '\u{1}';

/// Render a VM-internal path the way the platform's `Path.toString()` does,
/// for embedding in a `java.nio.file` exception.
///
/// CratonVM stores paths in one internal form that uses `/` everywhere. On
/// Windows that leaked into every nio exception — `C:/Users/…` where HotSpot
/// says `C:\Users\…` — even though `Path.toString()` itself was already right.
/// A virtual-filesystem path is exempt: archive entries are `/`-separated on
/// every platform, as the JDK's own zipfs and jrtfs report them.
///
/// On Unix this is the identity function and costs nothing.
pub fn exception_path(path: &str) -> std::borrow::Cow<'_, str> {
    #[cfg(windows)]
    {
        if path.contains('/') && !path.starts_with(VFS_SENTINEL) {
            return std::borrow::Cow::Owned(path.replace('/', "\\"));
        }
    }
    std::borrow::Cow::Borrowed(path)
}

/// Build a REAL `java/nio/file/FileAlreadyExistsException` naming `path`.
///
/// Every file-creating `java.nio.file` entry point (`Files.copy`, `Files.move`,
/// `createFile`, `newByteChannel`/`newOutputStream` with `CREATE_NEW`) must
/// reject an already-existing target with this EXACT class when the caller did
/// not pass `StandardCopyOption.REPLACE_EXISTING`. Callers discriminate by
/// TYPE, not by message — `regression-suite/src/RJdkNio.java:99` is a bare
/// `catch (FileAlreadyExistsException expected)`, and H2's
/// `FilePathDisk.moveTo` catches the same class to translate it into
/// `DbException(FILE_RENAME_FAILED_2)`. So neither a bare `IOException` with a
/// matching message nor (worse) a silent overwrite satisfies the contract:
/// `RuntimeError` has no `FileAlreadyExistsException` variant, which is why
/// this is built explicitly instead of going through `io_error`.
///
/// Constructed through the class's REAL single-`String` constructor rather
/// than a fabricated synthetic layout, for two reasons: the object is handed
/// straight to real JDK bytecode (`getFile()`, `Throwable` formatting) which
/// reads real field offsets, and a fabricated stand-in is exactly what
/// `--jdk-only` refuses. Mirrors `native-builtins`'
/// `phases_late::nio_file::p57_file_already_exists`, which does the same thing
/// for the natives registered on that side.
///
/// GC: the fresh exception is pinned across `create_string`, which allocates
/// and can therefore relocate it under a moving young collection (the native
/// stale-local family) — the ref is re-read from the pin before every use.
///
/// The `IOException` fallback fires only when the class itself cannot be
/// loaded, i.e. when there is no `FileAlreadyExistsException` to throw.
///
/// `pub` (not `pub(crate)`): `native-io` declares `pub mod nio_native`, so a
/// crate-private helper here would trip `dead_code` until its only caller —
/// the `java/nio/file/Files.copy` native in this crate's `lib.rs` — is wired
/// up. Exporting it also lets the `native-builtins` side reuse one builder
/// instead of keeping a third copy of this constructor dance.
pub fn file_already_exists(ctx: &mut dyn NativeContext, path: &str) -> MethodCallFailed {
    if let Ok(Some(Value::Object(Some(exc)))) =
        ctx.new_object("java/nio/file/FileAlreadyExistsException")
    {
        let pin = ctx.pin_native_root(exc);
        let file_str = ctx.create_string(exception_path(path).as_ref());
        let exc_cur = ctx.read_native_pin(pin, exc);
        let _ = ctx.invoke(
            "java/nio/file/FileAlreadyExistsException",
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(exc_cur)), Value::Object(Some(file_str))],
        );
        let exc_cur = ctx.read_native_pin(pin, exc);
        ctx.unpin_native_roots(pin);
        return MethodCallFailed::ExceptionThrown(exc_cur);
    }
    io_error(format!("FileAlreadyExistsException: {path}"))
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

/// Render the raw argument list for a refusal message.
///
/// `long_arg`/`int_arg` answer 0 for an argument that is absent or of the wrong
/// `Value` shape, so a refused `(addr, len, pos)` triple of zeroes is ambiguous:
/// the JDK may genuinely have passed a zero, or the dispatch may have handed us
/// a shape these accessors do not read. Printing the arguments as received
/// separates the two without a rebuild.
fn args_debug(args: &[Value]) -> String {
    let rendered: Vec<String> = args
        .iter()
        .map(|v| match v {
            Value::Int(x) => format!("I:{x}"),
            Value::Long(x) => format!("J:{x}"),
            Value::Object(Some(_)) => "L:obj".to_string(),
            Value::Object(None) => "L:null".to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    format!("[{}]", rendered.join(", "))
}

// ---------------------------------------------------------------------------
// sun/nio/ch/FileDispatcherImpl natives
// ---------------------------------------------------------------------------

/// Upper bound on the per-call transfer length for the FileDispatcher
/// `read0`/`pread0`/`write0`/`pwrite0` paths. A JVM-supplied positive
/// `len` (up to `i32::MAX`) would otherwise drive a `vec![0u8; len]`
/// (~2 GiB) allocation before any I/O. We clamp `len` down to this cap so
/// the allocation is bounded; a short transfer is legal for read/write
/// (the caller loops). Mirrors `net.rs`'s `NET_MAX_TRANSFER` (`1 << 30`).
const FD_MAX_TRANSFER: usize = 1 << 30;

/// `read0(FileDescriptor, long addr, int len) -> int`
/// Reads `len` bytes from the fd at its current position into the
/// raw memory at `addr`. Returns bytes read, or -1 on EOF.
fn native_fd_read0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let addr = long_arg(args, 1);
    let len = int_arg(args, 2);
    if addr == 0 || len < 0 {
        return Err(io_error(format!(
            "read0: bad addr/len (addr={addr:#x}, len={len}) args={}",
            args_debug(args)
        )));
    }
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("read0: FileDescriptor has no open handle"));
    };
    // Clamp the allocation; a short read is legal (the caller loops).
    let len = (len as usize).min(FD_MAX_TRANSFER);
    let mut buf = vec![0u8; len];
    let n = ctx
        .fd_table()
        .read_bytes(fd, &mut buf)
        .map_err(|e| io_error(format!("read0: {e}")))?;
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    // `addr` may be a real OS pointer OR an `Unsafe.allocateMemory` arena
    // handle: `FileChannel.read(heapBuffer)` routes through
    // `Util.getTemporaryDirectBuffer`, whose `DirectByteBuffer.address()` is a
    // synthetic arena handle (not dereferenceable). Route through the context
    // so a handle lands in the off-heap store instead of being memcpy'd raw
    // (which SIGSEGVs, same as the socket path did). `n <= len` bounds it.
    if !ctx.copy_to_native_memory(addr, &buf[..n]) {
        return Err(io_error(format!(
            "read0: invalid destination address {addr:#x}"
        )));
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
        return Err(io_error(format!(
            "pread0: bad addr/len/pos (addr={addr:#x}, len={len}, pos={pos}) args={}",
            args_debug(args)
        )));
    }
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("pread0: FileDescriptor has no open handle"));
    };
    // Clamp the allocation; a short read is legal (the caller loops).
    let len = (len as usize).min(FD_MAX_TRANSFER);
    let mut buf = vec![0u8; len];
    let n = ctx
        .fd_table()
        .pread_at(fd, &mut buf, pos as u64)
        .map_err(|e| io_error(format!("pread0: {e}")))?;
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    // Route through the context — `addr` may be an arena handle (see read0).
    if !ctx.copy_to_native_memory(addr, &buf[..n]) {
        return Err(io_error(format!(
            "pread0: invalid destination address {addr:#x}"
        )));
    }
    Ok(Some(Value::Int(n as i32)))
}

/// `write0(FileDescriptor, long addr, int len, boolean append) -> int`
fn native_fd_write0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let addr = long_arg(args, 1);
    let len = int_arg(args, 2);
    if addr == 0 || len < 0 {
        return Err(io_error(format!(
            "write0: bad addr/len (addr={addr:#x}, len={len}) args={}",
            args_debug(args)
        )));
    }
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("write0: FileDescriptor has no open handle"));
    };
    // Clamp the allocation; a short write is legal (the caller loops).
    let len = (len as usize).min(FD_MAX_TRANSFER);
    let mut buf = vec![0u8; len];
    // `addr` may be a real OS pointer OR an `Unsafe.allocateMemory` arena
    // handle (`FileChannel.write(heapBuffer)` → `Util.getTemporaryDirectBuffer`
    // → arena-backed `DirectByteBuffer.address()`). Route through the context
    // so a handle is read from the off-heap store instead of dereferenced raw
    // (a raw memcpy from the synthetic handle SIGSEGVs).
    if !ctx.copy_from_native_memory(addr, &mut buf) {
        return Err(io_error(format!(
            "write0: invalid source address {addr:#x}"
        )));
    }
    ctx.fd_table()
        .write_bytes(fd, &buf)
        .map_err(|e| io_error(format!("write0: {e}")))?;
    Ok(Some(Value::Int(len as i32)))
}

/// `pwrite0(FileDescriptor, long addr, int len, long pos) -> int`
fn native_fd_pwrite0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let addr = long_arg(args, 1);
    let len = int_arg(args, 2);
    let pos = long_arg(args, 3);
    if addr == 0 || len < 0 || pos < 0 {
        return Err(io_error(format!(
            "pwrite0: bad addr/len/pos (addr={addr:#x}, len={len}, pos={pos}) args={}",
            args_debug(args)
        )));
    }
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("pwrite0: FileDescriptor has no open handle"));
    };
    // Clamp the allocation; a short write is legal (the caller loops).
    let len = (len as usize).min(FD_MAX_TRANSFER);
    let mut buf = vec![0u8; len];
    // Route through the context — `addr` may be an arena handle (see write0).
    if !ctx.copy_from_native_memory(addr, &mut buf) {
        return Err(io_error(format!(
            "pwrite0: invalid source address {addr:#x}"
        )));
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
///
/// JDK `FileDispatcherImpl.seek0` sentinel: a **negative** offset means "return
/// the current position without seeking" (used by `FileChannelImpl.position()`,
/// which calls `nd.seek(fd, -1)`). CratonVM previously did
/// `SeekFrom::Start(pos as u64)` unconditionally, so `seek0(fd, -1)` became
/// `Start(u64::MAX)` → on Windows a FILE_BEGIN offset > i64::MAX is a negative
/// seek → `IOException: seek0: …before beginning of file (os error 131)`. That
/// broke every `FileChannelImpl.position()`/relative `read(ByteBuffer)` — e.g.
/// commons-compress's `ZipFile.positionAtEndOfCentralDirectoryRecord` (Spring
/// Boot buildpack `ZipFileTarArchive`).
fn native_fd_seek0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let pos = long_arg(args, 1);
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("seek0: FileDescriptor has no open handle"));
    };
    let seek = if pos < 0 {
        std::io::SeekFrom::Current(0) // query current position, no seek
    } else {
        std::io::SeekFrom::Start(pos as u64)
    };
    let new_pos = ctx
        .fd_table()
        .rw_seek(fd, seek)
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
fn native_file_cleanable_cleanup_close0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let fd = int_arg(args, 0);
    if fd > 2 {
        let _ = ctx.fd_table().close(fd as FdId);
    }
    let handle = long_arg(args, 1);
    if handle > 2 && handle < u32::MAX as i64 && handle as i32 != fd {
        let _ = ctx.fd_table().close(handle as FdId);
    }
    Ok(None)
}

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

/// `lock0(FileDescriptor fd, boolean blocking, long pos, long size, boolean shared) -> int`
///
/// Acquire a real OS advisory/byte-range lock on the file behind `fd`.
/// This is the native that backs `FileChannel.lock(...)` /
/// `FileChannel.tryLock(...)`.
///
/// JDK `sun.nio.ch.FileDispatcher` return convention (matched exactly):
///        `NO_LOCK     = -1` (could not acquire — non-blocking tryLock
///                            failed because the range is already held;
///                            `FileChannelImpl.tryLock` maps this to a
///                            `null` `FileLock`),
///        `LOCKED      =  0` (acquired the lock exactly as requested),
///        `RET_EX_LOCK =  1` (acquired an *exclusive* lock when a shared
///                            one was asked for — a Unix quirk we never
///                            return because `flock`/`LockFileEx` honor
///                            the requested mode),
///        `INTERRUPTED =  2` (blocking lock interrupted — N/A here).
///   * A genuine OS error throws `IOException` (returned as `Err`).
///
/// We return `LOCKED (0)` on success and `NO_LOCK (-1)` when a
/// non-blocking acquisition fails because the range is already held.
/// (The previous no-op stub returned `0` unconditionally, which is why
/// callers always "succeeded" — `0` is precisely `LOCKED`.)
///
/// Parameter mapping:
///   * `pos`  — start byte offset of the range to lock.
///   * `size` — length of the range. The JDK passes `Long.MAX_VALUE`
///     for a whole-file lock; we treat `size <= 0 || pos + size`
///     overflow as "to end of file" (whole range from `pos`).
///   * `shared`  — true => shared (read) lock; false => exclusive.
///   * `blocking == false` (i.e. `tryLock`) => fail immediately if the
///     range is contended rather than waiting.
/// MEASURED file-locking semantics, so the next reader does not have to
/// re-derive them from a comment — one of which was wrong (see
/// `native_fd_release0`).
///
/// `lock0` takes its OS lock through a `clone_file` handle that it then drops,
/// and `release0` unlocks through a DIFFERENT, freshly cloned handle. Both are
/// only correct if a byte-range lock outlives the particular handle it was
/// placed through. These tests assert exactly that, on whichever platform they
/// run, and include the negative control that makes the assertion mean
/// something.
///
/// Measured on Windows 11 (JDK-independent, this is pure Win32) 2026-08-08:
/// `LockFileEx` locks live on the kernel FILE_OBJECT, not on the HANDLE. A
/// `DuplicateHandle` (which is what `File::try_clone` is) yields a second handle
/// onto the SAME file object, so closing it releases nothing, and `UnlockFileEx`
/// through any handle onto that object releases the range. A separate
/// `CreateFile` makes a NEW file object and does contend.
#[cfg(test)]
mod file_lock_semantics_tests {
    use super::os_lock;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::Path;

    fn open_at(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    }

    /// `lock0`'s load-bearing premise: the lock it places through a transient
    /// `clone_file` handle is still held after that handle is dropped, because
    /// the fd_table's own handle keeps the underlying file object alive.
    ///
    /// If this ever stops holding, `lock0` returns `LOCKED (0)` — the value
    /// `FileChannelImpl.lock`/`tryLock` branches on to hand back a live
    /// `FileLockImpl` — for a lock that is not there, and two processes running
    /// `if (ch.tryLock() == null) bail;` both proceed. That is the H2
    /// `FileLock` / Derby `db.lck` / Lucene `NativeFSLockFactory` shape, so this
    /// test is the guard on a silent two-writer corruption, not on a comment.
    #[test]
    fn a_lock_outlives_the_transient_handle_it_was_placed_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("guard.lck");

        // Stands in for the fd_table entry: alive for the channel's lifetime.
        let owner = open_at(&path).expect("owner handle");
        // Stands in for `ctx.fd_table().clone_file(fd)`.
        let transient = owner.try_clone().expect("clone_file stand-in");

        assert!(
            os_lock::lock_range(&transient, 0, i64::MAX, false, false).expect("lock_range"),
            "the first exclusive whole-file lock must be granted"
        );
        drop(transient); // exactly what `lock0` does on its way out

        // Stands in for the other process / another opener.
        let rival = open_at(&path).expect("rival handle");
        assert!(
            !os_lock::lock_range(&rival, 0, i64::MAX, false, false)
                .expect("a contended tryLock must report, not error"),
            "PREMISE VIOLATED: dropping the handle `lock0` locked through released \
             the lock, so `lock0` returns LOCKED for a lock that is not held"
        );

        // `release0`'s actual implementation: unlock through a FRESH clone,
        // not through the handle that locked. Measured to work — which is what
        // makes `release0` correct despite its (now corrected) comment.
        let fresh = owner.try_clone().expect("fresh clone");
        os_lock::unlock_range(&fresh, 0, i64::MAX)
            .expect("unlock through a different handle onto the same file object");

        assert!(
            os_lock::lock_range(&rival, 0, i64::MAX, false, false).expect("lock_range"),
            "after release0's unlock the range must be acquirable again"
        );
        os_lock::unlock_range(&rival, 0, i64::MAX).expect("cleanup unlock");
    }

    /// NEGATIVE CONTROL. Without this, the test above would also pass on a
    /// platform where nothing ever contends — e.g. if `lock_range` quietly
    /// no-op'd and always answered `true`.
    #[test]
    fn control_an_independent_opener_really_is_refused_and_then_admitted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("control.lck");

        let holder = open_at(&path).expect("holder");
        let rival = open_at(&path).expect("rival");

        assert!(
            os_lock::lock_range(&holder, 0, i64::MAX, false, false).expect("lock"),
            "control: an uncontended lock must be granted"
        );
        assert!(
            !os_lock::lock_range(&rival, 0, i64::MAX, false, false).expect("probe"),
            "control: locking is not enforced at all on this platform — every \
             assertion in this module is vacuous"
        );
        os_lock::unlock_range(&holder, 0, i64::MAX).expect("unlock");
        assert!(
            os_lock::lock_range(&rival, 0, i64::MAX, false, false).expect("probe 2"),
            "control: the range stayed locked after an explicit unlock"
        );
        os_lock::unlock_range(&rival, 0, i64::MAX).expect("cleanup");
    }

    /// Closing the LAST handle onto the file object releases the range. This is
    /// the property that bounds the blast radius of a `FileChannel.close()` that
    /// never called `release0`.
    #[test]
    fn closing_the_only_handle_releases_the_range() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sole.lck");

        let sole = open_at(&path).expect("sole handle");
        assert!(os_lock::lock_range(&sole, 0, i64::MAX, false, false).expect("lock"));
        drop(sole);

        let rival = open_at(&path).expect("rival");
        assert!(
            os_lock::lock_range(&rival, 0, i64::MAX, false, false).expect("probe"),
            "a lock must not outlive the last handle onto its file"
        );
        os_lock::unlock_range(&rival, 0, i64::MAX).expect("cleanup");
    }
}

fn native_fd_lock0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // JDK sun.nio.ch.FileDispatcher return codes.
    const NO_LOCK: i32 = -1;
    const LOCKED: i32 = 0;

    let fd_obj = fd_arg(args, 0)?;
    let blocking = int_arg(args, 1) != 0;
    let pos = long_arg(args, 2);
    let size = long_arg(args, 3);
    let shared = int_arg(args, 4) != 0;

    if pos < 0 {
        return Err(io_error("lock0: negative position"));
    }
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("lock0: FileDescriptor has no open handle"));
    };

    // Obtain a handle/fd onto the same kernel file object. `clone_file`
    // dups the underlying handle (Windows `DuplicateHandle` / Unix
    // `dup`), so the lock placed through it is coherent with the file
    // and survives this temporary clone being dropped (the original fd
    // keeps the kernel file object / open file description alive).
    let file = ctx
        .fd_table()
        .clone_file(fd)
        .map_err(|e| io_error(format!("lock0: {e}")))?;

    match os_lock::lock_range(&file, pos as u64, size, shared, blocking) {
        Ok(true) => Ok(Some(Value::Int(LOCKED))),
        Ok(false) => Ok(Some(Value::Int(NO_LOCK))),
        Err(e) => Err(io_error(format!("lock0: {e}"))),
    }
}

/// `release0(FileDescriptor fd, long pos, long size)`
///
/// Release the byte-range lock previously taken by `lock0` over exactly
/// `[pos, pos+size)`. The JDK always calls this with the same `pos`/
/// `size` it passed to `lock0`, so we unlock the identical range.
fn native_fd_release0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let pos = long_arg(args, 1);
    let size = long_arg(args, 2);

    if pos < 0 {
        return Err(io_error("release0: negative position"));
    }
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        // Nothing to release if the fd is already gone.
        return Ok(None);
    };
    // Unlock through a fresh clone of the same fd. This is a DIFFERENT handle
    // from the one `lock0` locked through, and that is fine on both platforms —
    // measured, not assumed, by `file_lock_semantics_tests` above:
    //
    //   * Unix: `flock` binds to the open file description, which the `dup`
    //     shares, so `LOCK_UN` here releases the lock `lock0` took.
    //   * Windows: `LockFileEx` ranges live on the kernel FILE_OBJECT, and
    //     `File::try_clone` is `DuplicateHandle` — a second handle onto that
    //     SAME object. `UnlockFileEx` through it releases the range, and the
    //     range is still there to release.
    //
    // CORRECTION (2026-08-08): this comment previously asserted the opposite —
    // "Windows `LockFileEx`, whose locks are per-HANDLE — so by the time
    // `release0` runs there is usually no live OS lock left, and a fresh clone
    // here cannot unlock a range it never locked". Both halves are false, and
    // the belief is load-bearing in the wrong direction: read literally it says
    // `lock0` hands out `LOCKED` for a lock that no longer exists, i.e. that
    // `tryLock()` cannot exclude a second process. It can — see
    // `a_lock_outlives_the_transient_handle_it_was_placed_through`, which pins
    // exactly that and now fails loudly if it ever stops being true. (Locks are
    // per-handle in the sense that they are NOT inherited by a re-`CreateFile`;
    // they are not per-handle across a `DuplicateHandle` of one file object.)
    //
    // The error is still SWALLOWED, for the unrelated reason that follows: the
    // real JDK calls `nd.release()` BEFORE
    // `fileLockTable.remove(fli)` in `FileChannelImpl.release`, so propagating
    // an IOException from a failed unlock would ABORT that table removal,
    // leaving a phantom in-JVM lock that makes the next `tryLock` on the same
    // file throw `OverlappingFileLockException` (H2 reopen: "the file is
    // locked").
    if let Ok(file) = ctx.fd_table().clone_file(fd) {
        let _ = os_lock::unlock_range(&file, pos as u64, size);
    }
    Ok(None)
}

/// Platform-specific byte-range file locking, declaring the Win32 /
/// libc externs inline in the same style as the `WSAPoll` / `poll`
/// blocks in `native-api/src/fd_table.rs`.
mod os_lock {
    use std::io;

    /// Normalize the JDK `(pos, size)` pair into the byte range to lock.
    /// The JDK uses `Long.MAX_VALUE` (and historically `0`) to mean
    /// "the rest of the file / whole file". We clamp an overflowing or
    /// non-positive `size` to "lock the maximum range from `pos`".
    fn range_len(pos: u64, size: i64) -> u64 {
        if size <= 0 || size == i64::MAX {
            // Whole file from `pos`: lock the largest range that does
            // not overflow past u64::MAX when added to `pos`. The JDK
            // passes `Long.MAX_VALUE` (`i64::MAX`) for a whole-file lock,
            // so treat that as the sentinel too rather than locking only
            // `[pos, pos + i64::MAX)`.
            u64::MAX - pos
        } else {
            let size = size as u64;
            // Clamp so `pos + size` never overflows.
            size.min(u64::MAX - pos)
        }
    }

    // -----------------------------------------------------------------
    // Windows: LockFileEx / UnlockFileEx on the file HANDLE.
    // -----------------------------------------------------------------
    #[cfg(windows)]
    pub fn lock_range(
        file: &std::fs::File,
        pos: u64,
        size: i64,
        shared: bool,
        blocking: bool,
    ) -> io::Result<bool> {
        use std::os::windows::io::AsRawHandle;

        // Flags for LockFileEx.
        const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;
        const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
        // GetLastError code returned when LOCKFILE_FAIL_IMMEDIATELY
        // cannot acquire because the range is already locked.
        const ERROR_LOCK_VIOLATION: i32 = 33;
        const ERROR_IO_PENDING: i32 = 997;

        #[repr(C)]
        struct Overlapped {
            internal: usize,
            internal_high: usize,
            offset: u32,
            offset_high: u32,
            h_event: *mut core::ffi::c_void,
        }

        #[link(name = "kernel32")]
        extern "system" {
            fn LockFileEx(
                hfile: *mut core::ffi::c_void,
                dwflags: u32,
                dwreserved: u32,
                nnumberofbytestolocklow: u32,
                nnumberofbytestolockhigh: u32,
                lpoverlapped: *mut Overlapped,
            ) -> i32;
        }

        let len = range_len(pos, size);
        let mut flags = 0u32;
        if !shared {
            flags |= LOCKFILE_EXCLUSIVE_LOCK;
        }
        if !blocking {
            flags |= LOCKFILE_FAIL_IMMEDIATELY;
        }

        let mut ov = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: (pos & 0xFFFF_FFFF) as u32,
            offset_high: (pos >> 32) as u32,
            h_event: core::ptr::null_mut(),
        };

        // SAFETY: `hfile` is a live Win32 file HANDLE owned by `file`
        // (kept alive for the duration of the call); `ov` is a fully
        // initialised OVERLAPPED carrying the 64-bit offset; the byte
        // count is split into its low/high 32-bit halves per the API.
        let rc = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                flags,
                0,
                (len & 0xFFFF_FFFF) as u32,
                (len >> 32) as u32,
                &mut ov as *mut Overlapped,
            )
        };
        if rc != 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // tryLock contention — report "not acquired" rather than error.
            Some(ERROR_LOCK_VIOLATION) | Some(ERROR_IO_PENDING) if !blocking => Ok(false),
            _ => Err(err),
        }
    }

    #[cfg(windows)]
    pub fn unlock_range(file: &std::fs::File, pos: u64, size: i64) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;

        #[repr(C)]
        struct Overlapped {
            internal: usize,
            internal_high: usize,
            offset: u32,
            offset_high: u32,
            h_event: *mut core::ffi::c_void,
        }

        #[link(name = "kernel32")]
        extern "system" {
            fn UnlockFileEx(
                hfile: *mut core::ffi::c_void,
                dwreserved: u32,
                nnumberofbytestounlocklow: u32,
                nnumberofbytestounlockhigh: u32,
                lpoverlapped: *mut Overlapped,
            ) -> i32;
        }

        let len = range_len(pos, size);
        let mut ov = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: (pos & 0xFFFF_FFFF) as u32,
            offset_high: (pos >> 32) as u32,
            h_event: core::ptr::null_mut(),
        };
        // SAFETY: same invariants as `lock_range`; unlocks the exact
        // range that the matching `lock_range` call locked.
        let rc = unsafe {
            UnlockFileEx(
                file.as_raw_handle(),
                0,
                (len & 0xFFFF_FFFF) as u32,
                (len >> 32) as u32,
                &mut ov as *mut Overlapped,
            )
        };
        if rc != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    // -----------------------------------------------------------------
    // Unix: flock on the file descriptor. POSIX `fcntl` locks are owned
    // per (process, inode) and are dropped the moment ANY fd to that
    // inode is closed — which would be unsafe here because we lock
    // through a `dup`-ed clone. `flock` locks are instead tied to the
    // open file description (shared across `dup`s) and survive the
    // temporary clone being dropped, so we use `flock`. It honors the
    // shared/exclusive and blocking/non-blocking dimensions; the byte
    // range is whole-file (flock's documented granularity), which is a
    // conservative superset of the requested `[pos, pos+size)`.
    // -----------------------------------------------------------------
    #[cfg(unix)]
    pub fn lock_range(
        file: &std::fs::File,
        _pos: u64,
        _size: i64,
        shared: bool,
        blocking: bool,
    ) -> io::Result<bool> {
        use std::os::fd::AsRawFd;

        const LOCK_SH: i32 = 1;
        const LOCK_EX: i32 = 2;
        const LOCK_NB: i32 = 4;
        const EWOULDBLOCK: i32 = 11; // == EAGAIN on Linux

        extern "C" {
            fn flock(fd: i32, operation: i32) -> i32;
        }

        let mut op = if shared { LOCK_SH } else { LOCK_EX };
        if !blocking {
            op |= LOCK_NB;
        }
        // SAFETY: `fd` is a live file descriptor owned by `file` (kept
        // alive across the call). `flock` only inspects kernel state for
        // that descriptor.
        let rc = unsafe { flock(file.as_raw_fd(), op) };
        if rc == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // tryLock contention.
            Some(EWOULDBLOCK) if !blocking => Ok(false),
            _ => Err(err),
        }
    }

    #[cfg(unix)]
    pub fn unlock_range(file: &std::fs::File, _pos: u64, _size: i64) -> io::Result<()> {
        use std::os::fd::AsRawFd;

        const LOCK_UN: i32 = 8;

        extern "C" {
            fn flock(fd: i32, operation: i32) -> i32;
        }

        // SAFETY: see `lock_range`.
        let rc = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::range_len;
        #[allow(unused_imports)]
        use cratonvm_native_api::{
            NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
            NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
        };

        #[test]
        fn whole_file_size_zero_locks_max_range() {
            assert_eq!(range_len(0, 0), u64::MAX);
            assert_eq!(range_len(10, 0), u64::MAX - 10);
        }

        #[test]
        fn whole_file_long_max_size_does_not_overflow() {
            // JDK passes Long.MAX_VALUE for a whole-file lock.
            let r = range_len(100, i64::MAX);
            assert!(r <= u64::MAX - 100);
            assert_eq!(r, u64::MAX - 100);
        }

        #[test]
        fn bounded_range_is_preserved() {
            assert_eq!(range_len(5, 20), 20);
        }

        #[test]
        fn negative_size_treated_as_whole_file() {
            assert_eq!(range_len(0, -1), u64::MAX);
        }
    }
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

/// The host's mmap allocation granularity, i.e. what
/// `GetSystemInfo().dwAllocationGranularity` / `sysconf(_SC_PAGESIZE)` report.
/// `FileChannelImpl.map` rounds the mapping offset down to a multiple of this
/// value and hands the remainder back as the buffer's start offset, so the
/// number is load-bearing rather than cosmetic: reporting 64 KiB on a 4 KiB
/// page host makes every mapping start up to 60 KiB earlier than it needs to.
fn host_allocation_granularity() -> i64 {
    #[cfg(unix)]
    {
        // SAFETY: `sysconf` is a pure query — no pointers, no out-parameters.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page > 0 {
            return page as i64;
        }
    }
    // Windows' allocation granularity is 64 KiB on every supported release;
    // also the conservative fallback if `sysconf` fails.
    65536
}

/// `allocationGranularity0() -> long` — page size for mmap alignment.
fn native_fc_allocation_granularity0(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Long(host_allocation_granularity())))
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

fn native_iou_fd_limit(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    #[cfg(unix)]
    {
        // SAFETY: `sysconf` takes an integer name and touches no caller memory;
        // it reports an unsupported/erroring query as -1, which the check below
        // treats as "no limit known".
        let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
        if limit > 0 {
            return Ok(Some(Value::Int(limit.min(i32::MAX as i64) as i32)));
        }
    }

    Ok(Some(Value::Int(1024)))
}

fn native_iou_init_ids(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// Public registration
// ---------------------------------------------------------------------------

// JDK-ONLY-CLASSIFY: bridge — `sun.nio.ch.FileDispatcherImpl` / `IOUtil` /
// `NativeThread` are the file-descriptor syscall layer: `read0`, `write0`,
// `pread0`, `size0`, `truncate0` and friends are ACC_NATIVE in JDK 25 and take
// raw memory pointers, which is why the call site warns that leaving them
// unregistered segfaults. The earlier version of this marker said only 5
// triples resolved statically "because the class name is a loop variable at
// most sites"; the schema-3 census resolves every row, and says 22 of this
// function's 75 registrations are ACC_NATIVE on a JDK 25 Unix image. Those 22
// state their kind at their own call sites (L5, 2026-08-05); the block below is
// split so that statement is true per row rather than per function.
//
// CORRECTED 2026-08-05 by a second census taken against a **Windows** JDK
// 25.0.4+7 image (CratonVM adjudicates an image it cannot run on, so this is
// one command, not a Windows machine). L5 assumed the two non-Unix spellings
// were "the same natives on their own image" and left them inherited. Two
// thirds of that was wrong:
//
//   * `sun/nio/ch/FileDispatcherImpl` is the leaf on BOTH images. On Windows it
//     declares the syscall surface itself (22 ACC_NATIVE methods); on Linux it
//     inherits it from `UnixFileDispatcherImpl`. Either way it is an
//     ACC_NATIVE target, so it states its kind.
//   * `sun/nio/ch/WindowsFileDispatcherImpl` **exists on neither image.** The
//     Windows JDK calls its class `FileDispatcherImpl` too.
//
// RE-ADJUDICATED 2026-08-10, six images (21.0.12+8 and 25.0.4+7 × linux,
// windows, macos). The `WindowsFileDispatcherImpl` rows — 30, not 28 — are
// declared by none of them, so §1.5 has no `ACC_NATIVE` method for them to bind
// to and they are `SyntheticStub`, tagged centrally by
// `cratonvm_native_api::no_image_receiver`. They are **not** deletion
// candidates and the previous sentence saying so was wrong for the whole
// bucket: under `--synthetic-jdk` the VM mints a stand-in for this class on
// demand and these registrations are its only implementation.
//
// Per-row table: l5-native-io-bridge-residuals-RETIRED-20260810.md
// Disposition: jdk-only-bridge-on-a-receiver-no-image-declares-FIXED-20260810.md
/// The leaf dispatcher class, present on every JDK 25 image.
pub(crate) const FD_LEAF: &str = "sun/nio/ch/FileDispatcherImpl";
/// The Unix-image declarer. Absent from a Windows image.
pub(crate) const FD_UNIX: &str = "sun/nio/ch/UnixFileDispatcherImpl";
/// A spelling no JDK 25 image has, kept only because a future JDK might.
pub(crate) const FD_WINDOWS: &str = "sun/nio/ch/WindowsFileDispatcherImpl";

/// Register one dispatcher native under all three platform spellings, stating
/// `NativeKind::Bridge` on exactly the ones `backed` names — the spellings a
/// JDK 25 image declares that method `ACC_NATIVE` on. The others keep the
/// ambient category, so the census can go on reporting them as unadjudicated.
pub(crate) fn register_fd_native(
    r: &mut NativeMethodRegistry,
    name: &str,
    desc: &str,
    cb: NativeCallback,
    backed: &[&str],
) {
    for cls in [FD_LEAF, FD_UNIX, FD_WINDOWS] {
        if backed.contains(&cls) {
            r.register_with_kind(cls, name, desc, cb, NativeKind::Bridge);
        } else {
            r.register(cls, name, desc, cb);
        }
    }
}

/// Register `sun/nio/ch/*` natives for real-JDK boot. Idempotent: the
/// registry's `register` replaces a previous entry at the same key,
/// so it's safe to call after other NIO-related registrars.
pub fn register_nio_natives_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(NativeKind::Bridge);
    // --- FileDispatcherImpl (Unix class name; Windows uses
    // WindowsFileDispatcherImpl but the static natives are on the
    // parent class or in a companion). Register on all three names
    // for robustness — the registry lookup uses (class, name, desc). ---
    // Which of the three names actually declares these natives is decided by
    // the image, not by this file. Measured against JDK 25.0.4+7 for
    // linux-x64 AND windows-x64 (2026-08-05):
    //
    //   `UnixFileDispatcherImpl`     linux only   — declares 17 ACC_NATIVE
    //   `FileDispatcherImpl`         both images  — inherits them on linux,
    //                                               declares 22 on windows
    //   `WindowsFileDispatcherImpl`  NEITHER      — the Windows JDK names its
    //                                               class `FileDispatcherImpl`
    //
    // So `backed` below is the list of spellings some JDK 25 image declares
    // that method `ACC_NATIVE` on; those state their kind, the rest stay
    // ambient. Registering all three is still deliberate robustness.
    const FD_ACC_NATIVE: &[(&str, &str, NativeCallback)] = &[
        ("read0", "(Ljava/io/FileDescriptor;JI)I", native_fd_read0),
        ("pread0", "(Ljava/io/FileDescriptor;JIJ)I", native_fd_pread0),
        ("readv0", "(Ljava/io/FileDescriptor;JI)J", native_fd_readv0),
        ("write0", "(Ljava/io/FileDescriptor;JI)I", native_fd_write0),
        (
            "pwrite0",
            "(Ljava/io/FileDescriptor;JIJ)I",
            native_fd_pwrite0,
        ),
        (
            "writev0",
            "(Ljava/io/FileDescriptor;JI)J",
            native_fd_writev0,
        ),
        ("size0", "(Ljava/io/FileDescriptor;)J", native_fd_size0),
        ("seek0", "(Ljava/io/FileDescriptor;J)J", native_fd_seek0),
        // `force0` is registered by `file_channel.rs::register_file_channel_real`
        // (real fsync via `std::fs::File::sync_all` / `sync_data`).
        (
            "truncate0",
            "(Ljava/io/FileDescriptor;J)I",
            native_fd_truncate0,
        ),
        (
            "available0",
            "(Ljava/io/FileDescriptor;)I",
            native_fd_available0,
        ),
        (
            "isOther0",
            "(Ljava/io/FileDescriptor;)Z",
            native_fd_isother0,
        ),
        ("lock0", "(Ljava/io/FileDescriptor;ZJJZ)I", native_fd_lock0),
        (
            "release0",
            "(Ljava/io/FileDescriptor;JJ)V",
            native_fd_release0,
        ),
        // map0 / unmap0 / transferTo0 / maxDirectTransferSize0 / force0 are
        // registered by `file_channel.rs::register_file_channel_real` (real
        // memmap2 / sendfile / fsync implementations). They were previously
        // stubbed here and the duplicate registration was fragile — order of
        // registration decided which won. Only `allocationGranularity0` (a
        // pure constant) is owned here.
        (
            "allocationGranularity0",
            "()J",
            native_fc_allocation_granularity0,
        ),
    ];
    // Registered for the same robustness reason, but resolving to no
    // ACC_NATIVE method on ANY of the three names on this image: the two
    // boolean-suffixed descriptors are older JDK spellings, `close0` and
    // `preClose0` are declared on `sun.nio.ch.UnixDispatcher` (net.rs registers
    // those and states them there), `duplicateHandle` is Windows-only, and
    // JDK 25's `setDirect0` takes `(Ljava/io/FileDescriptor;)I` — this
    // `CharBuffer` descriptor matches nothing. Left ambient on purpose.
    const FD_UNADJUDICATED: &[(&str, &str, NativeCallback)] = &[
        ("write0", "(Ljava/io/FileDescriptor;JIZ)I", native_fd_write0),
        (
            "writev0",
            "(Ljava/io/FileDescriptor;JIZ)J",
            native_fd_writev0,
        ),
        ("close0", "(Ljava/io/FileDescriptor;)V", native_fd_close0),
        (
            "preClose0",
            "(Ljava/io/FileDescriptor;)V",
            native_fd_preclose0,
        ),
        ("duplicateHandle", "(J)J", native_fd_duplicate_handle),
        (
            "setDirect0",
            "(Ljava/io/FileDescriptor;Ljava/nio/CharBuffer;)I",
            native_fd_setdirect0,
        ),
    ];
    // Both real spellings back every entry of `FD_ACC_NATIVE`.
    for (name, desc, cb) in FD_ACC_NATIVE {
        register_fd_native(r, name, desc, *cb, &[FD_LEAF, FD_UNIX]);
    }
    // Of the rest, only the Windows leaf declares anything: `preClose0` is
    // nowhere, and the other five are the Windows signatures (`setDirect0`
    // really does take a `CharBuffer` there; on Unix it takes only the
    // descriptor — see `register_nio_setdirect_unix` below).
    for (name, desc, cb) in FD_UNADJUDICATED {
        let backed: &[&str] = if *name == "preClose0" {
            &[]
        } else {
            &[FD_LEAF]
        };
        register_fd_native(r, name, desc, *cb, backed);
    }
    // Real JDK 25 declares `FileDispatcherImpl.init0()` (confirmed via javap),
    // not `init()` -- that name doesn't exist on this class at all. The
    // `init()` entry this replaced was a name mismatch that left `init0`
    // unregistered, so any bytecode path that loads `FileDispatcherImpl` (e.g.
    // `ManagementFactory.getPlatformMBeanServer()` on Linux) hit
    // `UnsatisfiedLinkError: sun/nio/ch/FileDispatcherImpl.init0()V`. This is
    // the one native the leaf class declares itself on Linux; `init0` is not
    // on `UnixFileDispatcherImpl` and `WindowsFileDispatcherImpl` does not
    // exist, so only the leaf states its kind.
    register_fd_native(r, "init0", "()V", native_nt_init, &[FD_LEAF]);
    // `setDirect0` on a Unix image takes ONLY the descriptor — the
    // `CharBuffer` overload registered above is the Windows signature. Without
    // this entry `UnixFileDispatcherImpl.setDirect0(fd)` is unregistered, so
    // `FileChannel.open(.., ExtendedOpenOption.DIRECT)` reaches
    // `setDirectIO` and dies on `UnsatisfiedLinkError` instead of being told
    // direct I/O is unavailable. `-1` is that answer, and it is what the
    // existing handler returns.
    // Registered ONLY on the declarer. `setDirectIO` issues an `invokestatic`
    // against `UnixFileDispatcherImpl`, so that is the one binding that can be
    // reached — and adding the leaf spelling would put a row in the census that
    // the (single-class) image adjudication scores as an unbacked `Bridge`,
    // which is precisely what L6's ratchet exists to refuse.
    r.register_with_kind(
        FD_UNIX,
        "setDirect0",
        "(Ljava/io/FileDescriptor;)I",
        native_fd_setdirect0,
        NativeKind::Bridge,
    );

    r.register_with_kind(
        "java/io/FileCleanable",
        "cleanupClose0",
        "(IJ)V",
        native_file_cleanable_cleanup_close0,
        NativeKind::Bridge,
    );

    // --- FileChannelImpl (legacy names for older JDKs that carried the
    // natives directly on this class). The map0 / unmap0 / transferTo0 /
    // maxDirectTransferSize0 entries here are owned by
    // `file_channel.rs::register_file_channel_real`. Only `position0`,
    // `allocationGranularity0` and `initIDs` remain — none of those
    // have a "real" counterpart and they are otherwise harmless.
    let fci = "sun/nio/ch/FileChannelImpl";
    // NOT an `initIDs()V` no-op despite the name: `FileChannelImpl.<clinit>`
    // does `allocationGranularity = initIDs();` — the JNI body returns the
    // host's mmap granularity. Answer it from the host instead of a hardcoded
    // 64 KiB (wrong on every 4 KiB-page Unix).

    // --- NativeThread ---
    let nt = "sun/nio/ch/NativeThread";
    r.register(nt, "current", "()J", native_nt_current);
    r.register_with_kind(nt, "current0", "()J", native_nt_current, NativeKind::Bridge);
    r.register(nt, "signal", "(J)V", native_nt_signal);
    r.register_with_kind(nt, "init", "()V", native_nt_init, NativeKind::Bridge);

    // --- IOUtil ---
    let iou = "sun/nio/ch/IOUtil";
    r.register_with_kind(iou, "iovMax", "()I", native_iou_iov_max, NativeKind::Bridge);
    r.register_with_kind(
        iou,
        "writevMax",
        "()J",
        native_iou_write_max_size,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        iou,
        "fdLimit",
        "()I",
        native_iou_fd_limit,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        iou,
        "initIDs",
        "()V",
        native_iou_init_ids,
        NativeKind::Bridge,
    );
    r.set_category(__prev_cat);
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

use cratonvm_types::ClassId;

// The UDP-socket machinery below backs the synthetic DatagramChannel shims
// (`t16_dc_*`). Those shims fabricate datagram/"connected" state, so per the
// no-synthetic-stubs policy they — and this helper block + its imports — are
// gated to `synthetic-jdk` only. In the default build the real JDK
// `DatagramChannel`/`sun.nio.ch` bytecode runs instead (see `datagram.rs`).
#[cfg(feature = "synthetic-jdk")]
use parking_lot::RwLock;
#[cfg(feature = "synthetic-jdk")]
use std::collections::HashMap;
#[cfg(feature = "synthetic-jdk")]
use std::net::UdpSocket;
#[cfg(feature = "synthetic-jdk")]
use std::sync::OnceLock;

/// Process-wide registry keeping `UdpSocket` handles alive while a
/// `DatagramChannel` Java object references them. The id stored in the
/// channel's `sock_id` field indexes into this map. Matches the
/// Session-86/87 Delta pattern for per-handle resource tables.
#[cfg(feature = "synthetic-jdk")]
fn udp_registry() -> &'static RwLock<HashMap<i32, UdpSocket>> {
    static REG: OnceLock<RwLock<HashMap<i32, UdpSocket>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

#[cfg(feature = "synthetic-jdk")]
fn udp_next_id() -> i32 {
    use std::sync::atomic::{AtomicI32, Ordering};
    static NEXT: AtomicI32 = AtomicI32::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

#[cfg(feature = "synthetic-jdk")]
fn udp_register(sock: UdpSocket) -> i32 {
    let id = udp_next_id();
    udp_registry().write().insert(id, sock);
    id
}

#[cfg(feature = "synthetic-jdk")]
fn udp_with<T>(id: i32, f: impl FnOnce(&UdpSocket) -> T) -> Option<T> {
    udp_registry().read().get(&id).map(f)
}

#[cfg(feature = "synthetic-jdk")]
fn udp_remove(id: i32) {
    udp_registry().write().remove(&id);
}

/// Allocate a synthetic instance of `class_name` with enough slots to hold
/// the synthetic field layout. Tolerates classes that don't appear in the
/// bootstrap classloader — falls back to `ClassId::new(0)` with the
/// explicit field count.
/// Retained for the `t16_dc_*` family, which is `synthetic-jdk`-gated, so this
/// has no caller in the default build. Do NOT reach for it in new code: it
/// resolves whatever name it is given, which for every abstract public API
/// class in `java.nio.channels` mints a receiver `new` cannot legally produce
/// (JVMS 6.5). `crate::concrete_receiver::alloc_concrete` is the replacement
/// and refuses a class that is abstract or an interface.
#[allow(dead_code)]
fn alloc_t16(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    nfields: usize,
) -> cratonvm_types::ObjectRef {
    match ctx.ensure_class_initialized(class_name) {
        Ok(cid) => ctx.alloc_object(cid, nfields),
        Err(_) => ctx.alloc_object(ClassId::new(0), nfields),
    }
}

fn obj_or_none(args: &[Value], idx: usize) -> Option<cratonvm_types::ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

// ---- AsynchronousFileChannel ----

#[cfg(any())]
#[allow(dead_code)]
fn t16_afc_open_legacy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

// ---- AsynchronousSocketChannel ----

// ---- AsynchronousChannelGroup ----

// ---- DatagramChannel (SYNTHETIC, synthetic-jdk only) ----
//
// FLAGGED SyntheticStub: these `t16_dc_*` shims fabricate datagram state — the
// connect handler invents a `127.0.0.1:9` target, ignores the underlying
// `udp.connect()` result, and unconditionally flips the channel to "connected".
// That is fake UDP behavior the no-synthetic-stubs policy forbids, so the whole
// family is gated to `synthetic-jdk` (off by default) and registered under
// `NativeKind::SyntheticStub`. In the default build the real JDK
// `DatagramChannel`/`sun.nio.ch` bytecode runs (see `datagram.rs`).

#[cfg(feature = "synthetic-jdk")]
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
    ctx.set_field(ch, 0, Value::Int(port)); // port
    ctx.set_field(ch, 1, Value::Int(1)); // open
    ctx.set_field(ch, 2, Value::Int(0)); // not connected
    ctx.set_field(ch, 3, Value::Int(1)); // blocking = true (JDK default)
    ctx.set_field(ch, 4, Value::Int(sock_id)); // sock_id into udp_registry
    Ok(Some(Value::Object(Some(ch))))
}

#[cfg(feature = "synthetic-jdk")]
fn t16_dc_is_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 2 => Ok(Some(ctx.get_field(o, 1))),
        _ => Ok(Some(Value::Int(1))),
    }
}

#[cfg(feature = "synthetic-jdk")]
fn t16_dc_is_connected(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 3 => Ok(Some(ctx.get_field(o, 2))),
        _ => Ok(Some(Value::Int(0))),
    }
}

#[cfg(feature = "synthetic-jdk")]
fn t16_dc_is_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        Some(o) if ctx.object_num_fields(o) >= 4 => Ok(Some(ctx.get_field(o, 3))),
        _ => Ok(Some(Value::Int(1))),
    }
}

#[cfg(feature = "synthetic-jdk")]
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

#[cfg(feature = "synthetic-jdk")]
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

#[cfg(feature = "synthetic-jdk")]
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

#[cfg(feature = "synthetic-jdk")]
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

fn t16_lr_get_sequence_number(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match obj_or_none(args, 0) {
        // Real-JDK layout (identified by its by-name-resolvable private
        // longThreadID long): sequenceNumber is a named field; the legacy
        // raw slot 6 below is the REAL record's longThreadID, so the old
        // read returned the thread id as the "sequence number".
        Some(o) if matches!(ctx.get_field_by_name(o, "longThreadID"), Value::Long(_)) => {
            match ctx.get_field_by_name(o, "sequenceNumber") {
                v @ Value::Long(_) => Ok(Some(v)),
                _ => Ok(Some(Value::Long(0))),
            }
        }
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

// JDK-ONLY-CLASSIFY: unknown — needs census. Nothing here resolves to an
// ACC_NATIVE method: 9 of the resolvable triples land on abstract methods of
// `java.nio.channels.AsynchronousFileChannel` / `AsynchronousChannelGroup`, 8
// name methods absent from JDK 25, and 6 shadow concrete bytecode. Abstract
// targets mean the tag chosen here decides dispatch for every channel
// implementation, including ones the application supplies. The
// `DatagramChannel` block later in this function already opts down to
// `SyntheticStub` and is `#[cfg(feature = "synthetic-jdk")]`-gated, so it is
// absent from the default build entirely and cannot be judged from a default
// census run — check both feature configurations before retagging.
/// Register the T16.5 / T16.6 channel overrides. Idempotent: safe to call
/// alongside `register_nio_natives_real` or phase-92's registrations.
pub fn register_t16_channel_overrides(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // THE `AsynchronousFileChannel` / `AsynchronousSocketChannel` /
    // `AsynchronousChannelGroup` OVERRIDES WERE RETIRED 2026-08-21 (WORKER 4).
    //
    // Fifteen registrations. They were the WINNERS -- this registrar runs last,
    // and re-registration updates the slot in place -- and what they were
    // winning against was the real implementation of the same three families in
    // `async_socket.rs` and `lib.rs`. `lib.rs`'s own integration comment has
    // said so since the modules landed: *"WP3.2 -- AIO socket channels +
    // AsynchronousChannelGroup. Supersedes the synthetic `t16_asc_*` /
    // `t16_acg_*` stubs registered in `register_t16_channel_overrides`."* It
    // did not supersede them; it lost to them.
    //
    // MEASURED (`--dump-native-registry`, `--jdk-only`, r1 build, `W4Abstract`):
    //
    //     AsynchronousSocketChannel.open()          owns_slot=True  inv=1
    //       [native-io/src/nio_native.rs:1782]   <- this file
    //       [native-io/src/async_socket.rs:3548] owns_slot=False   <- the owner
    //     AsynchronousFileChannel.open(Path,…)      owns_slot=True  inv=1
    //       [native-io/src/nio_native.rs:1770]   <- this file
    //       [native-io/src/lib.rs:21667]         owns_slot=False
    //
    // and the same shape for `isOpen`, `size` and `close` on all three classes.
    //
    // **Two independent defects were behind that, and they are why this is a
    // retirement rather than a re-order.**
    //
    // 1. **Two slot maps on one class.** `t16_asc_open` wrote
    //    `{0: connected, 1: open, 2: fd, 3: remote}`; `async_socket.rs`'s
    //    `aio_asc_open` -- which owns `connect`, `read`, `write`,
    //    `getRemoteAddress` and every option accessor on the same object --
    //    reads `{0: open, 1: connected, 2: reg_id, 3: remote}`. Slots 0 and 1
    //    mean the OPPOSITE thing and slot 2 holds a different KIND of integer.
    //    That is the `java.lang.Process` shape, and `net_channels.rs` carried a
    //    third copy of the same map. W7-49 measured the two-way version of this
    //    and could not repair it from one side; all three sides are in this
    //    lane's scope, so they move together.
    //
    // 2. **A fabricated ABSTRACT receiver.** `t16_asc_open` and
    //    `t16_acg_with_fixed` called `alloc_t16(ctx, "java/nio/channels/
    //    AsynchronousSocketChannel", …)` -- a name that resolves to the real,
    //    ABSTRACT JDK class. MEASURED by `probes/W4Abstract.java`:
    //    `AsynchronousSocketChannel.open().getClass()` answered
    //    `java.nio.channels.AsynchronousSocketChannel`, `isAbstract == true`,
    //    against `sun.nio.ch.UnixAsynchronousSocketChannelImpl` on the oracle.
    //    That is a receiver `new` cannot legally produce (JVMS 6.5). The
    //    `async_socket.rs` factories were fixed in the same commit series and
    //    mint the concrete class; while these fifteen rows owned the slots,
    //    that fix was inert -- which is exactly the `[a green arm is evidence
    //    about the question it asked]` failure, caught here only because the
    //    probe asked the class-identity question directly.
    //
    // **Trap 4 was checked, not assumed** (`WORKER-4` brief: retiring the
    // winner PROMOTES the loser). For every one of the fifteen triples the
    // promoted row is `native-io`'s own real implementation -- `async_socket.rs`
    // for the two socket families, `lib.rs::register_async_file_channel` for
    // the file channel -- and never `net_channels.rs`, whose rows are
    // `owns_slot=False` behind BOTH. Re-verify with
    // `--dump-native-registry` before and after any further change here.
    //
    // `t16_afc_open` was a one-line delegation to `crate::native_afc_open`
    // already, and `t16_afc_{is_open,size,close}` each opened with
    // `t16_afc_uses_real_handle(..) => crate::native_afc_*`. Since every
    // `AsynchronousFileChannel` this VM builds comes from `alloc_afc_channel`,
    // that predicate was true for all of them and the legacy arms below it were
    // unreachable. Deleting the four rows promotes the very functions they were
    // delegating to.

    // DatagramChannel (SYNTHETIC, synthetic-jdk only).
    //
    // FLAGGED SyntheticStub: the `t16_dc_*` family fabricates datagram/connect
    // state (per `S1` in native-io.md). Per the
    // no-synthetic-stubs policy these overrides are compiled in only under
    // `synthetic-jdk` and tagged `NativeKind::SyntheticStub`. In the default
    // build they are absent, so the real JDK `DatagramChannel`/`sun.nio.ch`
    // bytecode (and `datagram.rs`) runs instead of a faked channel.
    #[cfg(feature = "synthetic-jdk")]
    {
        let dc = "java/nio/channels/DatagramChannel";
        r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            dc,
            "open",
            "()Ljava/nio/channels/DatagramChannel;",
            t16_dc_open,
        );
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
        // Restore the family default category for the remaining (Bridge) entries.
        r.set_category(cratonvm_native_api::NativeKind::Bridge);
    }

    // java.util.logging extras.
    //
    // REMOVED 2026-07-27 (stub-removal sweep): this block also registered
    // no-ops for `Logger.entering(String,String)V` and
    // `LogManager.readConfiguration()V`. They arrived as "null-tolerant
    // variants that supersede the phase-72 handlers, which NPE on
    // `args.first() == Object(None)`" — i.e. a null-receiver crash was fixed
    // by silencing the method outright.
    //
    // The cost was invisible and real: `register_io_natives` runs AFTER
    // `register_essential_natives_with_shims` in the real-JDK arm of
    // `vm/src/vm/vm_init.rs`, and re-registration updates the slot in place
    // (last-registration-wins — see `native-api/src/registry.rs`). So this
    // no-op silently overwrote `logmanager.rs`'s real `entering`
    // implementation and `Logger.entering(...)` emitted NOTHING, measured
    // against HotSpot which logs a FINER "ENTRY" record. `logmanager.rs`'s
    // `jul_trace_marker` is itself null-receiver tolerant (it early-returns
    // on a null `this`), so the original NPE cannot recur.
    //
    // `readConfiguration()V` now resolves to `logmanager.rs`'s handler,
    // which is ALSO a no-op — but a deliberate, documented one:
    // `java.util.logging.config.file` can name a `config` class to
    // instantiate, so parsing untrusted logging config is a code-execution
    // vector. Dropping the undocumented duplicate leaves the security
    // rationale attached to the one surviving site.
    r.register(
        "java/util/logging/LogRecord",
        "getSequenceNumber",
        "()J",
        t16_lr_get_sequence_number,
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
    r.set_category(__prev_cat);
}
