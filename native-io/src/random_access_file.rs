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

use parking_lot::Mutex;

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use rustjvm_types::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// Handle table
// ---------------------------------------------------------------------------

/// Module-local handle table: integer id -> open File.
///
/// Starts at 1000 to stay clear of the 0..3 stdio reservations and the
/// small fd_table space (which starts at 3 and rarely exceeds a few
/// hundred in practice).  A collision would only cause a wrong-file
/// error; it wouldn't be a memory-safety issue because the two systems
/// never read each other's ids.
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1000);

fn handle_map() -> &'static Mutex<HashMap<i64, File>> {
    static MAP: std::sync::OnceLock<Mutex<HashMap<i64, File>>> = std::sync::OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn alloc_handle(file: File) -> i64 {
    let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    handle_map().lock().insert(h, file);
    h
}

fn with_file<F, R>(handle: i64, f: F) -> Option<R>
where
    F: FnOnce(&mut File) -> R,
{
    let mut map = handle_map().lock();
    map.get_mut(&handle).map(f)
}

fn remove_handle(handle: i64) -> Option<File> {
    handle_map().lock().remove(&handle)
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
    if let Some(fd_obj) = raf_fd_object(ctx, this) {
        // Store the same id in both slots so code looking at either
        // gets a consistent value.  JDK's RandomAccessFile close path
        // checks `fd.fd != -1`.
        ctx.set_field_by_name(fd_obj, "fd", Value::Int(handle as i32));
        ctx.set_field_by_name(fd_obj, "handle", Value::Long(handle));
    }
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

    let mut opts = OpenOptions::new();
    // O_RDONLY == 1 means read-only; O_RDWR == 2 means read+write.
    // Java spec: "rw" => O_RDWR; "r" => O_RDONLY.  The private open()
    // Java wrapper already translates the string mode into these bits.
    opts.read(true);
    if mode_bits & O_RDWR != 0 {
        opts.write(true).create(true);
    }
    // O_SYNC / O_DSYNC are best-effort; std::fs doesn't expose sync
    // flags portably. Leave as a no-op.
    let _ = (O_SYNC, O_DSYNC);

    let file = opts.open(&path_str).map_err(|_| fnf(&path_str))?;
    let handle = alloc_handle(file);
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
    let off = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
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
    for (i, &b) in buf[..n].iter().enumerate() {
        ctx.set_array_element(arr, off + i, Value::Int(b as i32));
    }
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
        Some(Ok(())) => Ok(None),
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
    let off = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    if len == 0 {
        return Ok(None);
    }
    let mut buf = vec![0u8; len];
    for i in 0..len {
        buf[i] = match ctx.get_array_element(arr, off + i) {
            Value::Int(v) => v as u8,
            _ => 0,
        };
    }
    let handle = match read_handle(ctx, this) {
        Some(h) => h,
        None => return Ok(None),
    };
    match with_file(handle, |f| f.write_all(&buf)) {
        Some(Ok(())) => Ok(None),
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
    let pos = match args.get(1) {
        Some(Value::Long(v)) => *v,
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
        let h = alloc_handle(file);
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
        let h = alloc_handle(file);

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
}
