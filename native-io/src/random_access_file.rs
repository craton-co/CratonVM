// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! java.io.RandomAccessFile JNI-style natives for real-JDK mode.
//!
//! These implement the private native delegates that the JDK 25
//! RandomAccessFile bytecode invokes:
//!
//! - open0(String, int) throws FileNotFoundException
//! - read0() -> int
//! - readBytes0(byte[], int, int) -> int
//! - write0(int)
//! - writeBytes0(byte[], int, int)
//! - getFilePointer() -> long
//! - seek0(long)
//! - length0() -> long
//! - setLength0(long)
//! - close0()
//! - initIDs() (no-op)
//!
//! Handles are stored in `RandomAccessFile.fd.fd` as an i32 handle id
//! (mirroring the pattern used by FileInputStream.open0 in lib.rs where
//! the fd slot of `this` holds an integer id).  The actual
//! `std::fs::File` is kept in a process-global map keyed by that id.
//!
//! Synthetic-mode uses the fd_table-based `<init>`/`read`/`write`
//! overrides in `lib.rs::register_io_extras_natives`; those remain
//! unchanged.  The two systems never collide because they register
//! different method names.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// Handle table
// ---------------------------------------------------------------------------

/// Java spec for `RandomAccessFile` modes:
///   "rws" => O_SYNC  → every write syncs data + metadata (`sync_all`)
///   "rwd" => O_DSYNC → every write syncs data only      (`sync_data`)
/// Per-write sync is required by the spec; performance is the caller's
/// problem (they explicitly asked for durable writes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncMode {
    /// `O_DSYNC` — `sync_data()` after each write (data only).
    Data,
    /// `O_SYNC`  — `sync_all()`  after each write (data + metadata).
    Full,
}

/// Per-handle state: the open `File` plus the sync mode requested at
/// open time. `None` sync_mode means no per-write sync (plain "r" or
/// "rw" modes).
///
/// AUDIT 2026-05-17: mirrors the `fd_table::FileEntry` pattern. The
/// underlying `File` lives behind its own `Arc<Mutex<_>>` so callers
/// can clone the handle out of the global map, drop the map-level
/// lock, and only then perform the blocking I/O. Holding the map
/// lock across the syscall serializes every RAF op in the VM against
/// the longest-running I/O on any open RAF.
struct RafHandle {
    file: Arc<Mutex<File>>,
    sync_mode: Option<SyncMode>,
}

impl Clone for RafHandle {
    fn clone(&self) -> Self {
        Self {
            file: Arc::clone(&self.file),
            sync_mode: self.sync_mode,
        }
    }
}

/// Module-local handle table: integer id -> RafHandle.
///
/// Starts at 1000 to stay clear of the 0..3 stdio reservations and the
/// small fd_table space (which starts at 3 and rarely exceeds a few
/// hundred in practice).  A collision would only cause a wrong-file
/// error; it wouldn't be a memory-safety issue because the two systems
/// never read each other's ids.
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1000);

fn handle_map() -> &'static Mutex<HashMap<i64, RafHandle>> {
    static MAP: std::sync::OnceLock<Mutex<HashMap<i64, RafHandle>>> = std::sync::OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn alloc_handle(file: File, sync_mode: Option<SyncMode>) -> i64 {
    let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    handle_map().lock().insert(
        h,
        RafHandle {
            file: Arc::new(Mutex::new(file)),
            sync_mode,
        },
    );
    h
}

/// AUDIT 2026-05-17 (mirror of `fd_table::get_entry`): take the
/// map-level lock briefly to clone the per-handle `Arc<Mutex<File>>`,
/// drop the map lock, then lock and operate on the inner file. This
/// keeps the global handle-table lock off the blocking I/O path.
fn with_file<F, R>(handle: i64, f: F) -> Option<R>
where
    F: FnOnce(&mut File) -> R,
{
    let entry = handle_map().lock().get(&handle).cloned()?;
    let mut file = entry.file.lock();
    Some(f(&mut file))
}

/// Look up the configured sync mode for an open handle.
fn handle_sync_mode(handle: i64) -> Option<SyncMode> {
    handle_map().lock().get(&handle).and_then(|h| h.sync_mode)
}

/// Run an fsync corresponding to the handle's sync mode. No-op if
/// the handle was opened in a non-sync mode ("r" / "rw").
///
/// AUDIT 2026-05-17: same Arc-clone-then-drop pattern as `with_file`
/// so the fsync syscall does not serialize against handle-map mutations.
fn sync_for_handle(handle: i64) -> Result<(), std::io::Error> {
    let entry = match handle_map().lock().get(&handle).cloned() {
        Some(e) => e,
        None => return Ok(()),
    };
    let mode = match entry.sync_mode {
        Some(m) => m,
        None => return Ok(()),
    };
    let mut file = entry.file.lock();
    match mode {
        SyncMode::Data => file.sync_data(),
        SyncMode::Full => file.sync_all(),
    }
}

fn remove_handle(handle: i64) -> Option<Arc<Mutex<File>>> {
    handle_map().lock().remove(&handle).map(|h| h.file)
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn io_err(e: std::io::Error) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
        message: e.to_string(),
    }))
}

fn fnf(path: &str) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::FileNotFoundException {
        path: path.to_string(),
    }))
}

/// Validate caller-supplied `(off, len)` against a byte array of length
/// `arr_len`, matching the JDK's `Objects.checkFromIndexSize` contract
/// used by `RandomAccessFile.readBytes`/`writeBytes`.
///
/// Returns `Err(IndexOutOfBoundsException)` if `off` or `len` is negative
/// or if `off + len` exceeds `arr_len`. Uses checked arithmetic so a
/// caller-supplied `off + len` cannot overflow `usize`.
fn check_array_bounds(off: i32, len: i32, arr_len: usize) -> Result<(), MethodCallFailed> {
    if off < 0 || len < 0 {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ArrayIndexOutOfBoundsException {
                index: if off < 0 { off } else { len },
            },
        )));
    }
    match (off as usize).checked_add(len as usize) {
        Some(end) if end <= arr_len => Ok(()),
        _ => Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ArrayIndexOutOfBoundsException {
                index: off.saturating_add(len),
            },
        ))),
    }
}

// ---------------------------------------------------------------------------
// Field helpers
// ---------------------------------------------------------------------------

/// Resolve the RandomAccessFile.fd field (java.io.FileDescriptor) and
/// return its ObjectRef.  If it doesn't exist yet (shouldn't happen —
/// the Java constructor allocates it before calling open0), return None.
fn raf_fd_object(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(this, "fd") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// Read the integer handle id from a FileDescriptor object.  JDK 25
/// FileDescriptor has both `fd` (int) and `handle` (long on Windows).
/// We store our id in `fd` so all platforms use the same slot.
fn read_handle(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i64> {
    let fd_obj = raf_fd_object(ctx, this)?;
    match ctx.get_field_by_name(fd_obj, "fd") {
        Value::Int(v) if v != 0 => Some(v as i64),
        _ => {
            // Fallback to `handle` (long) — used by Windows JDK builds.
            match ctx.get_field_by_name(fd_obj, "handle") {
                Value::Long(v) if v != 0 => Some(v),
                _ => None,
            }
        }
    }
}

fn write_handle(ctx: &mut dyn NativeContext, this: ObjectRef, handle: i64) {
    // Real `RandomAccessFile.<init>` sets `this.fd = new FileDescriptor()` before
    // calling open0, but on CratonVM that constructor field-initializer does not
    // always land — `this.fd` reads back null (same class of issue as
    // java.net.Socket's null `socketLock`). With no FileDescriptor there is
    // nowhere to record the open handle, so `read_handle` returns None and every
    // length/read/seek behaves as a closed/empty file: `length()`=0, `read()`=-1,
    // and commons-compress's seek-from-EOF computes a negative offset
    // ("seek before beginning of file"). Create the FileDescriptor on demand.
    let fd_obj = match raf_fd_object(ctx, this) {
        Some(o) => o,
        None => match ctx.new_object("java/io/FileDescriptor") {
            Ok(Some(Value::Object(Some(fd)))) => {
                ctx.set_field_by_name(this, "fd", Value::Object(Some(fd)));
                fd
            }
            _ => return,
        },
    };
    // Store the same id in both slots so code looking at either
    // gets a consistent value.  JDK's RandomAccessFile close path
    // checks `fd.fd != -1`.
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(handle as i32));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(handle));
}

fn clear_handle(ctx: &mut dyn NativeContext, this: ObjectRef) {
    if let Some(fd_obj) = raf_fd_object(ctx, this) {
        ctx.set_field_by_name(fd_obj, "fd", Value::Int(-1));
        ctx.set_field_by_name(fd_obj, "handle", Value::Long(-1));
    }
}

// ---------------------------------------------------------------------------
// Native implementations
// ---------------------------------------------------------------------------

// JDK 25 java.io.RandomAccessFile mode bits (O_RDONLY / O_RDWR / O_SYNC / O_DSYNC).
const O_RDONLY: i32 = 1;
const O_RDWR: i32 = 2;
const O_SYNC: i32 = 4;
const O_DSYNC: i32 = 8;
// O_TEMPORARY = 16 — request the file be deleted on close; we ignore it.

#[allow(non_snake_case)]
fn native_initIDs(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn native_open0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (RandomAccessFile), args[1] = path (String), args[2] = mode (int)
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "RandomAccessFile.open0: missing this".to_string(),
            }))
        }
    };
    let path_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let mode_bits = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => O_RDONLY,
    };

    if path_str.is_empty() {
        return Err(fnf(&path_str));
    }

    // SECURITY (HIGH): run the crate-wide path validator BEFORE the open
    // syscall. Under `set_path_confine_to_cwd(true)` this rejects
    // `../../etc/passwd`-style escapes; without confinement it still
    // rejects literal `..` segments and null bytes. The validator's
    // `SecurityException` is translated to `FileNotFoundException` so
    // the Java-visible failure matches what RAF would throw for any
    // other unreadable file (RandomAccessFile.open0's declared throws).
    let path_str = match crate::validate_path(&path_str) {
        Ok(p) => p,
        Err(_) => return Err(fnf(&path_str)),
    };

    let mut opts = OpenOptions::new();
    // O_RDONLY == 1 means read-only; O_RDWR == 2 means read+write.
    // Java spec: "rw" => O_RDWR; "r" => O_RDONLY.  The private open()
    // Java wrapper already translates the string mode into these bits.
    opts.read(true);
    if mode_bits & O_RDWR != 0 {
        opts.write(true).create(true);
    }
    // Translate O_SYNC / O_DSYNC into a per-handle sync mode that the
    // write paths honour. std::fs doesn't expose O_SYNC portably at
    // open time, so we emulate by calling sync_all / sync_data after
    // every byte/buffer write. Per Java spec for "rws"/"rwd" this is
    // mandatory for durability — performance is the caller's choice.
    let sync_mode = if mode_bits & O_SYNC != 0 {
        Some(SyncMode::Full)
    } else if mode_bits & O_DSYNC != 0 {
        Some(SyncMode::Data)
    } else {
        None
    };

    let file = opts.open(&path_str).map_err(|_| fnf(&path_str))?;
    let handle = alloc_handle(file, sync_mode);
    write_handle(ctx, this, handle);
    Ok(None)
}

fn native_read0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let handle = match read_handle(ctx, this) {
        Some(h) => h,
        None => return Ok(Some(Value::Int(-1))),
    };
    let res = with_file(handle, |f| {
        let mut buf = [0u8; 1];
        match f.read(&mut buf) {
            Ok(0) => Ok(-1i32),
            Ok(_) => Ok(buf[0] as i32),
            Err(e) => Err(e),
        }
    });
    match res {
        Some(Ok(v)) => Ok(Some(Value::Int(v))),
        Some(Err(e)) => Err(io_err(e)),
        None => Ok(Some(Value::Int(-1))),
    }
}

#[allow(non_snake_case)]
fn native_readBytes0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // Bounds-check caller-supplied off/len against the array before
    // handing them to the bulk-write intrinsic (mirrors JDK RAF.readBytes).
    check_array_bounds(off_i, len_i, ctx.array_length(arr))?;
    let off = off_i as usize;
    let len = len_i as usize;
    if len == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let handle = match read_handle(ctx, this) {
        Some(h) => h,
        None => return Ok(Some(Value::Int(-1))),
    };
    let mut buf = vec![0u8; len];
    let n = match with_file(handle, |f| f.read(&mut buf)) {
        Some(Ok(n)) => n,
        Some(Err(e)) => return Err(io_err(e)),
        None => return Ok(Some(Value::Int(-1))),
    };
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    // AUDIT 2026-05-17: bulk write via NativeContext intrinsic.
    ctx.write_byte_array_from(arr, off, &buf[..n]);
    Ok(Some(Value::Int(n as i32)))
}

fn native_write0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let byte = match args.get(1) {
        Some(Value::Int(v)) => *v as u8,
        _ => return Ok(None),
    };
    let handle = match read_handle(ctx, this) {
        Some(h) => h,
        None => return Ok(None),
    };
    match with_file(handle, |f| f.write_all(&[byte])) {
        Some(Ok(())) => {
            // Honour O_SYNC / O_DSYNC: per Java RandomAccessFile spec,
            // "rws" and "rwd" modes require that every write reach
            // stable storage before the call returns.
            sync_for_handle(handle).map_err(io_err)?;
            Ok(None)
        }
        Some(Err(e)) => Err(io_err(e)),
        None => Ok(None),
    }
}

#[allow(non_snake_case)]
fn native_writeBytes0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // Bounds-check caller-supplied off/len against the array before
    // handing them to the bulk-read intrinsic (mirrors JDK RAF.writeBytes).
    check_array_bounds(off_i, len_i, ctx.array_length(arr))?;
    let off = off_i as usize;
    let len = len_i as usize;
    if len == 0 {
        return Ok(None);
    }
    let mut buf = vec![0u8; len];
    // AUDIT 2026-05-17: bulk read via NativeContext intrinsic.
    ctx.read_byte_array_into(arr, off, &mut buf);
    let handle = match read_handle(ctx, this) {
        Some(h) => h,
        None => return Ok(None),
    };
    match with_file(handle, |f| f.write_all(&buf)) {
        Some(Ok(())) => {
            // Honour O_SYNC / O_DSYNC — see native_write0 above.
            sync_for_handle(handle).map_err(io_err)?;
            Ok(None)
        }
        Some(Err(e)) => Err(io_err(e)),
        None => Ok(None),
    }
}

#[allow(non_snake_case)]
fn native_getFilePointer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let handle = match read_handle(ctx, this) {
        Some(h) => h,
        None => return Ok(Some(Value::Long(0))),
    };
    match with_file(handle, |f| f.stream_position()) {
        Some(Ok(p)) => Ok(Some(Value::Long(p as i64))),
        Some(Err(e)) => Err(io_err(e)),
        None => Ok(Some(Value::Long(0))),
    }
}

fn native_seek0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Long arguments crossing native boundaries can arrive tagged as
    // Double (same 64-bit payload, different Value tag). Reinterpret bits.
    let pos = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Double(v)) => i64::from_le_bytes(v.to_le_bytes()),
        _ => 0,
    };
    if pos < 0 {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IOException {
                message: "Negative seek offset".to_string(),
            },
        )));
    }
    let handle = match read_handle(ctx, this) {
        Some(h) => h,
        None => return Ok(None),
    };
    match with_file(handle, |f| f.seek(SeekFrom::Start(pos as u64))) {
        Some(Ok(_)) => Ok(None),
        Some(Err(e)) => Err(io_err(e)),
        None => Ok(None),
    }
}

fn native_length0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let handle = match read_handle(ctx, this) {
        Some(h) => h,
        None => return Ok(Some(Value::Long(0))),
    };
    match with_file(handle, |f| f.metadata().map(|m| m.len())) {
        Some(Ok(n)) => Ok(Some(Value::Long(n as i64))),
        Some(Err(e)) => Err(io_err(e)),
        None => Ok(Some(Value::Long(0))),
    }
}

#[allow(non_snake_case)]
fn native_setLength0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let new_len = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Double(v)) => i64::from_le_bytes(v.to_le_bytes()),
        _ => 0,
    };
    if new_len < 0 {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IOException {
                message: "Negative length".to_string(),
            },
        )));
    }
    let handle = match read_handle(ctx, this) {
        Some(h) => h,
        None => return Ok(None),
    };
    match with_file(handle, |f| f.set_len(new_len as u64)) {
        Some(Ok(())) => Ok(None),
        Some(Err(e)) => Err(io_err(e)),
        None => Ok(None),
    }
}

fn native_close0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Some(handle) = read_handle(ctx, this) {
        // Drop the File to close it
        let _ = remove_handle(handle);
    }
    clear_handle(ctx, this);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_random_access_file_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let raf = "java/io/RandomAccessFile";
    registry.register(raf, "initIDs", "()V", native_initIDs);
    registry.register(raf, "open0", "(Ljava/lang/String;I)V", native_open0);
    registry.register(raf, "read0", "()I", native_read0);
    registry.register(raf, "readBytes0", "([BII)I", native_readBytes0);
    registry.register(raf, "write0", "(I)V", native_write0);
    registry.register(raf, "writeBytes0", "([BII)V", native_writeBytes0);
    registry.register(raf, "getFilePointer", "()J", native_getFilePointer);
    registry.register(raf, "seek0", "(J)V", native_seek0);
    registry.register(raf, "length0", "()J", native_length0);
    registry.register(raf, "setLength0", "(J)V", native_setLength0);
    registry.register(raf, "close0", "()V", native_close0);
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_table_roundtrips_open_read_close() {
        use std::io::Write as _;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"hello world").unwrap();
        let path = tmp.path().to_path_buf();

        let file = File::open(&path).unwrap();
        let h = alloc_handle(file, None);
        assert!(handle_map().lock().contains_key(&h));

        let mut buf = [0u8; 5];
        let n = with_file(h, |f| f.read(&mut buf)).unwrap().unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b"hello");

        let removed = remove_handle(h);
        assert!(removed.is_some());
        assert!(!handle_map().lock().contains_key(&h));
    }

    #[test]
    fn seek_and_length_via_handle() {
        use std::io::Write as _;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"0123456789").unwrap();
        let file = File::open(tmp.path()).unwrap();
        let h = alloc_handle(file, None);

        let len = with_file(h, |f| f.metadata().unwrap().len()).unwrap();
        assert_eq!(len, 10);

        with_file(h, |f| f.seek(SeekFrom::Start(3)).unwrap()).unwrap();
        let mut buf = [0u8; 3];
        with_file(h, |f| f.read_exact(&mut buf).unwrap()).unwrap();
        assert_eq!(&buf, b"345");

        remove_handle(h);
    }

    #[test]
    fn open_options_read_only_rejects_write() {
        use std::io::Write as _;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"x").unwrap();
        let mut opts = OpenOptions::new();
        opts.read(true);
        let mut f = opts.open(tmp.path()).unwrap();
        assert!(f.write_all(b"y").is_err());
    }

    /// HIGH-severity security regression guard: under
    /// `set_path_confine_to_cwd(true)`, `RandomAccessFile.open0` must
    /// reject a relative-path traversal *before* opening the file. The
    /// Java-visible failure must be `FileNotFoundException` (matching
    /// `open0`'s declared throws), not a `SecurityException` leaking
    /// out of the native into a confused Java caller.
    #[test]
    fn raf_open0_rejects_relative_traversal_under_confinement() {
        use crate::test_support::MockNativeContext;
        use cratonvm_native_api::NativeContext;

        let _g = crate::test_support::confine_test_lock().lock();
        crate::set_path_confine_to_cwd(true);

        // Build minimal `this`: an object whose `fd` field references
        // another (empty) FileDescriptor object. open0 will call
        // `raf_fd_object` on `this` via `get_field_by_name("fd")` —
        // our mock returns `Value::Object(None)` from that, so the
        // fd_object lookup yields None and `write_handle` becomes a
        // no-op. That doesn't matter here: validation runs and rejects
        // BEFORE any handle is written.
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(1);
        let path = ctx.attach_string("../../etc/passwd");
        let args = [
            Value::Object(Some(this)),
            Value::Object(Some(path)),
            Value::Int(1), // O_RDONLY
        ];

        let r = native_open0(&mut ctx, &args);

        crate::set_path_confine_to_cwd(false);

        assert!(r.is_err(), "RAF.open0 accepted traversal path");
        let err = format!("{:?}", r.unwrap_err());
        assert!(
            err.contains("FileNotFoundException"),
            "expected FileNotFoundException, got: {err}"
        );
        // Confirm the SecurityException did NOT leak out — the entry
        // point's contract is `throws FileNotFoundException` only.
        assert!(
            !err.contains("SecurityException"),
            "SecurityException leaked out of RAF.open0: {err}"
        );
    }
}
