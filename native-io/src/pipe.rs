// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP3.7 — anonymous pipe via `java.nio.channels.Pipe.open()`.
//!
//! Acceptance criterion: a Java program can call `Pipe.open()`, then
//! write bytes to `pipe.sink()` and read them back from `pipe.source()`.
//!
//! Backing kernel objects:
//!
//!   * **Linux/macOS/BSD**: real anonymous pipe via `libc::pipe(2)`.
//!     Returns two file descriptors (read end, write end).  We stash
//!     each as a u64 in our process-wide `pipe_table` and surface a
//!     synthetic Java `SourceChannelImpl` / `SinkChannelImpl` wrapping
//!     the id.
//!   * **Windows**: real anonymous pipe via the Win32 `CreatePipe`
//!     entry, declared with raw `extern "system"` FFI against
//!     `Kernel32` (same pattern as `native-builtins/src/servlet.rs`'s
//!     `WSAPoll`, avoids pulling in `windows-sys` as a dependency).
//!     Returns two `HANDLE`s (`HANDLE` is `*mut c_void`, stored as
//!     u64).  Same wrapping pattern; the read/write paths use
//!     `ReadFile` / `WriteFile` with overlapped=NULL for blocking
//!     semantics (matching the JDK's `SourceChannel.read` /
//!     `SinkChannel.write` blocking default).
//!
//! Because we cannot edit `lib.rs` to share a unified fd_table, this
//! module owns its own `pipe_table` keyed by a u32 id.  The Java
//! channel objects we hand back stash that id at field 0 and use it on
//! every subsequent native call.
//!
//! Thread safety: every kernel handle / fd in the table is read/written
//! from concurrent threads, but the underlying read/write syscalls are
//! thread-safe at the kernel level (the kernel serializes per-fd I/O).
//! The table itself is guarded by a `RwLock`.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{OnceLock, RwLock};
use std::collections::HashMap;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Pipe handle representation
// ---------------------------------------------------------------------------

/// A pipe end identifier — opaque to Java, just a key in our table.
/// We wrap raw OS handles (fd or HANDLE) so the table is platform-agnostic.
#[derive(Copy, Clone)]
struct PipeEnd {
    /// Raw OS handle.  On Unix this is an `int` (the fd); on Windows
    /// it's a `HANDLE` (a pointer).  Stored as u64 so a single field
    /// covers both.
    raw: u64,
    /// `true` for the writable (sink) end, `false` for the readable
    /// (source) end.  Used to validate that read/write goes through
    /// the right side.
    is_sink: bool,
    /// Closed flag — guards against double-close from finalizers.
    closed: bool,
}

fn pipe_table() -> &'static RwLock<HashMap<i32, PipeEnd>> {
    static T: OnceLock<RwLock<HashMap<i32, PipeEnd>>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(HashMap::new()))
}

fn next_pipe_id() -> i32 {
    static N: AtomicI32 = AtomicI32::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn register_pipe_end(end: PipeEnd) -> i32 {
    let id = next_pipe_id();
    if let Ok(mut g) = pipe_table().write() {
        g.insert(id, end);
    }
    id
}

fn close_pipe_end(id: i32) -> bool {
    let mut g = match pipe_table().write() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if let Some(end) = g.get_mut(&id) {
        if !end.closed {
            end.closed = true;
            close_raw(end.raw);
            return true;
        }
    }
    false
}

fn pipe_end_get(id: i32) -> Option<PipeEnd> {
    pipe_table().read().ok()?.get(&id).copied()
}

// ---------------------------------------------------------------------------
// Platform-specific kernel interactions
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod platform {
    use super::PipeEnd;

    pub(super) fn create_anonymous_pipe() -> std::io::Result<(PipeEnd, PipeEnd)> {
        let mut fds: [libc::c_int; 2] = [0, 0];
        // SAFETY: `pipe(2)` writes two ints into the array we pass.
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((
            PipeEnd {
                raw: fds[0] as u64,
                is_sink: false,
                closed: false,
            },
            PipeEnd {
                raw: fds[1] as u64,
                is_sink: true,
                closed: false,
            },
        ))
    }

    pub(super) fn read_pipe(raw: u64, buf: &mut [u8]) -> std::io::Result<isize> {
        // SAFETY: `read(2)` is thread-safe; we pass our own buffer.
        let n = unsafe {
            libc::read(
                raw as libc::c_int,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as libc::size_t,
            )
        };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as isize)
        }
    }

    pub(super) fn write_pipe(raw: u64, buf: &[u8]) -> std::io::Result<isize> {
        // SAFETY: `write(2)` is thread-safe.
        let n = unsafe {
            libc::write(
                raw as libc::c_int,
                buf.as_ptr() as *const libc::c_void,
                buf.len() as libc::size_t,
            )
        };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as isize)
        }
    }

    pub(super) fn close_raw(raw: u64) {
        // SAFETY: best-effort close; ignore errors (matches JDK).
        unsafe {
            libc::close(raw as libc::c_int);
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::PipeEnd;
    use std::ffi::c_void;

    // Raw Win32 FFI — same convention as `native-builtins/src/servlet.rs`'s
    // WSAPoll declaration, avoids pulling in `windows-sys` as a dep.
    type Handle = *mut c_void;
    type Bool = i32;

    #[link(name = "Kernel32")]
    extern "system" {
        fn CreatePipe(
            h_read_pipe: *mut Handle,
            h_write_pipe: *mut Handle,
            lp_pipe_attributes: *const c_void, // SECURITY_ATTRIBUTES; NULL OK
            n_size: u32,
        ) -> Bool;
        fn ReadFile(
            h_file: Handle,
            lp_buffer: *mut c_void,
            n_number_of_bytes_to_read: u32,
            lp_number_of_bytes_read: *mut u32,
            lp_overlapped: *mut c_void, // OVERLAPPED; NULL = synchronous
        ) -> Bool;
        fn WriteFile(
            h_file: Handle,
            lp_buffer: *const c_void,
            n_number_of_bytes_to_write: u32,
            lp_number_of_bytes_written: *mut u32,
            lp_overlapped: *mut c_void,
        ) -> Bool;
        fn CloseHandle(h_object: Handle) -> Bool;
    }

    pub(super) fn create_anonymous_pipe() -> std::io::Result<(PipeEnd, PipeEnd)> {
        let mut hread: Handle = std::ptr::null_mut();
        let mut hwrite: Handle = std::ptr::null_mut();
        // SAFETY: `CreatePipe(read_handle, write_handle, NULL_attr,
        // 0_default_size)` — we pass null SECURITY_ATTRIBUTES (handles
        // not inheritable; Java pipes don't expose handles to child
        // processes anyway) and 0 for default 4 KiB buffer.
        let ok = unsafe { CreatePipe(&mut hread, &mut hwrite, std::ptr::null(), 0) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((
            PipeEnd {
                raw: hread as u64,
                is_sink: false,
                closed: false,
            },
            PipeEnd {
                raw: hwrite as u64,
                is_sink: true,
                closed: false,
            },
        ))
    }

    pub(super) fn read_pipe(raw: u64, buf: &mut [u8]) -> std::io::Result<isize> {
        let mut read: u32 = 0;
        // SAFETY: ReadFile is thread-safe per-handle.  Synchronous call
        // with NULL OVERLAPPED — blocks until the pipe has data or the
        // write end is closed (in which case it returns 0 = EOF).
        let ok = unsafe {
            ReadFile(
                raw as Handle,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // Treat ERROR_BROKEN_PIPE (109) as clean EOF.
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(109) {
                return Ok(0);
            }
            return Err(err);
        }
        Ok(read as isize)
    }

    pub(super) fn write_pipe(raw: u64, buf: &[u8]) -> std::io::Result<isize> {
        let mut written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                raw as Handle,
                buf.as_ptr() as *const c_void,
                buf.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(written as isize)
    }

    pub(super) fn close_raw(raw: u64) {
        // SAFETY: best-effort close; ignore errors.
        unsafe {
            CloseHandle(raw as Handle);
        }
    }
}

// On non-unix-non-windows platforms, fail at runtime with IOException
// rather than silently noop — keeps the surface honest.
#[cfg(not(any(unix, windows)))]
mod platform {
    use super::PipeEnd;
    pub(super) fn create_anonymous_pipe() -> std::io::Result<(PipeEnd, PipeEnd)> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Pipe.open: platform not supported",
        ))
    }
    pub(super) fn read_pipe(_: u64, _: &mut [u8]) -> std::io::Result<isize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Pipe.read: platform not supported",
        ))
    }
    pub(super) fn write_pipe(_: u64, _: &[u8]) -> std::io::Result<isize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Pipe.write: platform not supported",
        ))
    }
    pub(super) fn close_raw(_: u64) {}
}

use platform::{close_raw, create_anonymous_pipe, read_pipe, write_pipe};

// ---------------------------------------------------------------------------
// Java-side helpers
// ---------------------------------------------------------------------------

fn io_error(message: impl Into<String>) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
        message: message.into(),
    }))
}

/// Pipe channel layout (3 fields):
///   slot 0: id          (Int)  — index into pipe_table
///   slot 1: open_flag   (Int)  — 1 = open, 0 = closed
///   slot 2: is_sink     (Int)  — 1 = sink, 0 = source
const PIPE_FIELD_ID: usize = 0;
const PIPE_FIELD_OPEN: usize = 1;
const PIPE_FIELD_KIND: usize = 2;

/// Pipe wrapper layout (2 fields):
///   slot 0: source  (Object) — SourceChannelImpl
///   slot 1: sink    (Object) — SinkChannelImpl
const PIPE_WRAPPER_FIELD_SOURCE: usize = 0;
const PIPE_WRAPPER_FIELD_SINK: usize = 1;

fn alloc_channel(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    is_sink: bool,
    id: i32,
) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized(class_name)
        .unwrap_or_else(|_| ClassId::new(0));
    // 4 fields gives us a couple of spare slots for potential
    // SelectableChannel state added later — keeps layout forgiving.
    let obj = ctx.alloc_object(cid, 4);
    ctx.set_field(obj, PIPE_FIELD_ID, Value::Int(id));
    ctx.set_field(obj, PIPE_FIELD_OPEN, Value::Int(1));
    ctx.set_field(obj, PIPE_FIELD_KIND, Value::Int(if is_sink { 1 } else { 0 }));
    ctx.set_field(obj, 3, Value::Int(1)); // blocking = true
    obj
}

fn arg_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn arg_int(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        Some(Value::Long(v)) => *v as i32,
        _ => 0,
    }
}

/// Extract the underlying byte-array + offset/length from a Java
/// `ByteBuffer`-shaped object.  Tolerates several layouts:
///   * Heap ByteBuffer: field "hb" or slot 5 holds a `byte[]`.
///   * Position is at field "position" or slot 0.
///   * Limit is at field "limit" or slot 1.
/// Returns (array_ref, position, limit).  None if no recognizable layout.
fn buffer_view(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<(ObjectRef, i32, i32)> {
    let arr = match ctx.get_field_by_name(buf, "hb") {
        Value::Object(Some(a)) => a,
        _ => match ctx.get_field(buf, 5) {
            Value::Object(Some(a)) => a,
            _ => return None,
        },
    };
    let position = match ctx.get_field_by_name(buf, "position") {
        Value::Int(v) => v,
        _ => match ctx.get_field(buf, 0) {
            Value::Int(v) => v,
            _ => 0,
        },
    };
    let limit = match ctx.get_field_by_name(buf, "limit") {
        Value::Int(v) => v,
        _ => match ctx.get_field(buf, 1) {
            Value::Int(v) => v,
            _ => 0,
        },
    };
    Some((arr, position, limit))
}

fn buffer_set_position(ctx: &mut dyn NativeContext, buf: ObjectRef, new_pos: i32) {
    ctx.set_field_by_name(buf, "position", Value::Int(new_pos));
    ctx.set_field(buf, 0, Value::Int(new_pos));
}

// ---------------------------------------------------------------------------
// Native handlers
// ---------------------------------------------------------------------------

/// `java.nio.channels.Pipe.open()` — allocate a new (source, sink)
/// pair backed by a real kernel pipe.  Returns the wrapper.
fn pipe_open(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let (read_end, write_end) =
        create_anonymous_pipe().map_err(|e| io_error(format!("Pipe.open: {e}")))?;
    let read_id = register_pipe_end(read_end);
    let write_id = register_pipe_end(write_end);
    let source = alloc_channel(ctx, "sun/nio/ch/SourceChannelImpl", false, read_id);
    let sink = alloc_channel(ctx, "sun/nio/ch/SinkChannelImpl", true, write_id);

    let pipe_cid = ctx
        .ensure_class_initialized("java/nio/channels/Pipe")
        .unwrap_or_else(|_| ClassId::new(0));
    let wrapper = ctx.alloc_object(pipe_cid, 2);
    ctx.set_field(
        wrapper,
        PIPE_WRAPPER_FIELD_SOURCE,
        Value::Object(Some(source)),
    );
    ctx.set_field(wrapper, PIPE_WRAPPER_FIELD_SINK, Value::Object(Some(sink)));
    Ok(Some(Value::Object(Some(wrapper))))
}

/// `java.nio.channels.Pipe.source()` — return the cached source channel.
fn pipe_source(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Object(None)));
    };
    if ctx.object_num_fields(this) >= 1 {
        return Ok(Some(ctx.get_field(this, PIPE_WRAPPER_FIELD_SOURCE)));
    }
    Ok(Some(Value::Object(None)))
}

/// `java.nio.channels.Pipe.sink()` — return the cached sink channel.
fn pipe_sink(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Object(None)));
    };
    if ctx.object_num_fields(this) >= 2 {
        return Ok(Some(ctx.get_field(this, PIPE_WRAPPER_FIELD_SINK)));
    }
    Ok(Some(Value::Object(None)))
}

/// `sun.nio.ch.SinkChannelImpl.write(ByteBuffer)` — write the buffer's
/// `[position, limit)` slice to the pipe.  Returns the number of bytes
/// actually written and advances `position` accordingly.
fn sink_write_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("SinkChannel.write: null this"));
    };
    let id = match ctx.get_field(this, PIPE_FIELD_ID) {
        Value::Int(v) => v,
        _ => return Err(io_error("SinkChannel.write: missing pipe id")),
    };
    let end = pipe_end_get(id).ok_or_else(|| io_error("SinkChannel.write: closed or unknown"))?;
    if end.closed {
        return Err(io_error("SinkChannel.write: channel closed"));
    }
    if !end.is_sink {
        return Err(io_error("SinkChannel.write: wrong end (source registered as sink)"));
    }
    let Some(buf) = arg_obj(args, 1) else {
        return Err(io_error("SinkChannel.write: null buffer"));
    };
    let Some((arr, position, limit)) = buffer_view(ctx, buf) else {
        return Err(io_error("SinkChannel.write: unrecognised ByteBuffer layout"));
    };
    if position >= limit {
        return Ok(Some(Value::Int(0)));
    }
    let to_write = (limit - position) as usize;
    // AUDIT 2026-05-17: bulk read via NativeContext intrinsic.
    let mut bytes = vec![0u8; to_write];
    ctx.read_byte_array_into(arr, position as usize, &mut bytes);
    let n = write_pipe(end.raw, &bytes).map_err(|e| io_error(format!("write: {e}")))?;
    if n > 0 {
        buffer_set_position(ctx, buf, position + n as i32);
    }
    Ok(Some(Value::Int(n as i32)))
}

/// `sun.nio.ch.SourceChannelImpl.read(ByteBuffer)` — read up to
/// `limit - position` bytes from the pipe into the buffer.  Returns
/// the number of bytes read or -1 on EOF.
fn source_read_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("SourceChannel.read: null this"));
    };
    let id = match ctx.get_field(this, PIPE_FIELD_ID) {
        Value::Int(v) => v,
        _ => return Err(io_error("SourceChannel.read: missing pipe id")),
    };
    let end = pipe_end_get(id).ok_or_else(|| io_error("SourceChannel.read: closed or unknown"))?;
    if end.closed {
        return Err(io_error("SourceChannel.read: channel closed"));
    }
    if end.is_sink {
        return Err(io_error("SourceChannel.read: wrong end (sink registered as source)"));
    }
    let Some(buf) = arg_obj(args, 1) else {
        return Err(io_error("SourceChannel.read: null buffer"));
    };
    let Some((arr, position, limit)) = buffer_view(ctx, buf) else {
        return Err(io_error("SourceChannel.read: unrecognised ByteBuffer layout"));
    };
    let space = (limit - position).max(0) as usize;
    if space == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let mut bytes = vec![0u8; space];
    let n = read_pipe(end.raw, &mut bytes).map_err(|e| io_error(format!("read: {e}")))?;
    if n == 0 {
        // EOF — JDK signals -1.
        return Ok(Some(Value::Int(-1)));
    }
    // AUDIT 2026-05-17: bulk write via NativeContext intrinsic.
    let copy_len = (n as usize).min(bytes.len());
    ctx.write_byte_array_from(arr, position as usize, &bytes[..copy_len]);
    buffer_set_position(ctx, buf, position + n as i32);
    Ok(Some(Value::Int(n as i32)))
}

/// `sun.nio.ch.SinkChannelImpl.write(byte[], int off, int len)` — direct
/// byte-array variant used by some JDK internal paths and helpful for
/// tests that don't want to allocate a real ByteBuffer.
fn sink_write_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("SinkChannel.write[bytes]: null this"));
    };
    let id = match ctx.get_field(this, PIPE_FIELD_ID) {
        Value::Int(v) => v,
        _ => return Err(io_error("SinkChannel.write[bytes]: missing pipe id")),
    };
    let end = pipe_end_get(id).ok_or_else(|| io_error("SinkChannel.write[bytes]: closed"))?;
    if end.closed || !end.is_sink {
        return Err(io_error("SinkChannel.write[bytes]: not a sink"));
    }
    let Some(arr) = arg_obj(args, 1) else {
        return Err(io_error("SinkChannel.write[bytes]: null array"));
    };
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let alen = ctx.array_length(arr);
    if off.saturating_add(len) > alen {
        return Err(io_error(format!(
            "SinkChannel.write[bytes]: out of bounds off={off} len={len} alen={alen}"
        )));
    }
    let mut bytes = vec![0u8; len];
    // AUDIT 2026-05-17: bulk read via NativeContext intrinsic.
    ctx.read_byte_array_into(arr, off, &mut bytes);
    let n = write_pipe(end.raw, &bytes).map_err(|e| io_error(format!("write: {e}")))?;
    Ok(Some(Value::Int(n as i32)))
}

/// `sun.nio.ch.SourceChannelImpl.read(byte[], int off, int len)`.
fn source_read_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("SourceChannel.read[bytes]: null this"));
    };
    let id = match ctx.get_field(this, PIPE_FIELD_ID) {
        Value::Int(v) => v,
        _ => return Err(io_error("SourceChannel.read[bytes]: missing pipe id")),
    };
    let end = pipe_end_get(id).ok_or_else(|| io_error("SourceChannel.read[bytes]: closed"))?;
    if end.closed || end.is_sink {
        return Err(io_error("SourceChannel.read[bytes]: not a source"));
    }
    let Some(arr) = arg_obj(args, 1) else {
        return Err(io_error("SourceChannel.read[bytes]: null array"));
    };
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let alen = ctx.array_length(arr);
    if off.saturating_add(len) > alen {
        return Err(io_error(format!(
            "SourceChannel.read[bytes]: out of bounds off={off} len={len} alen={alen}"
        )));
    }
    let mut bytes = vec![0u8; len];
    let n = read_pipe(end.raw, &mut bytes).map_err(|e| io_error(format!("read: {e}")))?;
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    // AUDIT 2026-05-17: bulk write via NativeContext intrinsic.
    let copy_len = (n as usize).min(bytes.len());
    ctx.write_byte_array_from(arr, off, &bytes[..copy_len]);
    Ok(Some(Value::Int(n as i32)))
}

fn channel_is_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Int(0)));
    };
    if ctx.object_num_fields(this) > PIPE_FIELD_OPEN {
        return Ok(Some(ctx.get_field(this, PIPE_FIELD_OPEN)));
    }
    Ok(Some(Value::Int(1)))
}

fn channel_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(None);
    };
    if ctx.object_num_fields(this) > PIPE_FIELD_OPEN {
        ctx.set_field(this, PIPE_FIELD_OPEN, Value::Int(0));
    }
    if let Value::Int(id) = ctx.get_field(this, PIPE_FIELD_ID) {
        close_pipe_end(id);
    }
    Ok(None)
}

fn channel_configure_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Object(None)));
    };
    let blocking = arg_int(args, 1);
    if ctx.object_num_fields(this) > 3 {
        ctx.set_field(this, 3, Value::Int(blocking));
    }
    Ok(Some(Value::Object(Some(this))))
}

// ---------------------------------------------------------------------------
// Public registration
// ---------------------------------------------------------------------------

/// Register the WP3.7 Pipe natives.  Idempotent.
pub fn register_pipe_real(r: &mut NativeMethodRegistry) {
    let pipe = "java/nio/channels/Pipe";
    r.register(pipe, "open", "()Ljava/nio/channels/Pipe;", pipe_open);
    r.register(pipe, "source", "()Ljava/nio/channels/Pipe$SourceChannel;", pipe_source);
    r.register(pipe, "sink", "()Ljava/nio/channels/Pipe$SinkChannel;", pipe_sink);

    // SourceChannelImpl
    let source = "sun/nio/ch/SourceChannelImpl";
    r.register(
        source,
        "read",
        "(Ljava/nio/ByteBuffer;)I",
        source_read_buffer,
    );
    r.register(source, "read", "([BII)I", source_read_bytes);
    r.register(source, "isOpen", "()Z", channel_is_open);
    r.register(source, "close", "()V", channel_close);
    r.register(
        source,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        channel_configure_blocking,
    );

    // Abstract SourceChannel — same implementations, different
    // declared class so Java-side dispatch lands here either way.
    let abstract_source = "java/nio/channels/Pipe$SourceChannel";
    r.register(
        abstract_source,
        "read",
        "(Ljava/nio/ByteBuffer;)I",
        source_read_buffer,
    );
    r.register(abstract_source, "isOpen", "()Z", channel_is_open);
    r.register(abstract_source, "close", "()V", channel_close);

    // SinkChannelImpl
    let sink = "sun/nio/ch/SinkChannelImpl";
    r.register(sink, "write", "(Ljava/nio/ByteBuffer;)I", sink_write_buffer);
    r.register(sink, "write", "([BII)I", sink_write_bytes);
    r.register(sink, "isOpen", "()Z", channel_is_open);
    r.register(sink, "close", "()V", channel_close);
    r.register(
        sink,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        channel_configure_blocking,
    );

    let abstract_sink = "java/nio/channels/Pipe$SinkChannel";
    r.register(
        abstract_sink,
        "write",
        "(Ljava/nio/ByteBuffer;)I",
        sink_write_buffer,
    );
    r.register(abstract_sink, "isOpen", "()Z", channel_is_open);
    r.register(abstract_sink, "close", "()V", channel_close);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wp37_pipe_create_close_kernel_level() {
        // Direct kernel-level round-trip — bypass the Java plumbing.
        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        assert!(!read_end.is_sink);
        assert!(write_end.is_sink);
        let payload: &[u8] = b"hello pipe";
        let n = write_pipe(write_end.raw, payload).expect("write");
        assert_eq!(n as usize, payload.len());
        let mut got = vec![0u8; payload.len()];
        let r = read_pipe(read_end.raw, &mut got).expect("read");
        assert_eq!(r as usize, payload.len());
        assert_eq!(&got, payload);
        close_raw(read_end.raw);
        close_raw(write_end.raw);
    }

    #[test]
    fn wp37_pipe_table_register_and_close() {
        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        let r_id = register_pipe_end(read_end);
        let w_id = register_pipe_end(write_end);
        assert!(r_id > 0 && w_id > 0 && r_id != w_id);
        let r = pipe_end_get(r_id).expect("read end registered");
        assert!(!r.is_sink);
        let w = pipe_end_get(w_id).expect("write end registered");
        assert!(w.is_sink);
        assert!(close_pipe_end(r_id));
        assert!(close_pipe_end(w_id));
        // Idempotent — second close returns false (already closed).
        assert!(!close_pipe_end(r_id));
    }

    #[test]
    fn wp37_pipe_table_unknown_id_returns_none() {
        assert!(pipe_end_get(i32::MAX - 1).is_none());
        assert!(!close_pipe_end(i32::MAX - 1));
    }

    #[test]
    fn wp37_eof_after_writer_close() {
        let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
        // Close write end immediately — read should observe EOF.
        close_raw(write_end.raw);
        let mut buf = [0u8; 32];
        let r = read_pipe(read_end.raw, &mut buf).expect("read at EOF");
        assert_eq!(r, 0, "expected EOF");
        close_raw(read_end.raw);
    }
}
