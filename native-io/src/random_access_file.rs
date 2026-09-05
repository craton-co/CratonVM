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
//! ## Single FD registry (TC0622 fix)
//!
//! A `RandomAccessFile` and any `FileChannel` obtained from it
//! (`raf.getChannel()`) share **one** `java.io.FileDescriptor` — and on
//! HotSpot they share one OS fd. CratonVM's nio natives
//! (`FileDispatcherImpl.size0/read0/…`, `FileChannelImpl.map0`) resolve a
//! `FileDescriptor` against the global `fd_table` (`native-api`). So the RAF
//! natives MUST register the open file in that **same** `fd_table` and store
//! the returned `FdId` on the descriptor — otherwise `raf.getChannel().size()`
//! / `.map(...)` fail with `IOException: size0: bad fd for size`.
//!
//! `open0` therefore opens through `fd_table().open_random_access(path,
//! write)` (a `FileReadWrite` entry, which `file_size`/`rw_read`/`rw_seek`/
//! `clone_file` all handle), and `read0`/`write0`/`seek0`/`length0`/… drive
//! that same fd. The id is written into both `fd.fd` (int) and `fd.handle`
//! (long) so either layout (Unix `fd` / Windows `handle`) resolves it. This
//! replaces the previous module-local handle table, whose ids (≥ 1000) were
//! invisible to the nio path and overlapped the `fd_table` id range.
//!
//! Synthetic-mode uses the fd_table-based `<init>`/`read`/`write`
//! overrides in `lib.rs::register_io_extras_natives`; those remain
//! unchanged.
//!
//! **They collide on exactly one name, and the claim that they "never collide
//! because they register different method names" was false for three years.**
//! `getFilePointer()J` is on both lists, because it is the one JDK 25 RAF
//! method that is simultaneously public API (so the synthetic block
//! re-implements it) and `ACC_NATIVE` (so this module bridges it). This
//! registrar runs six lines after the synthetic one and used to register that
//! row unconditionally, so the synthetic arm never got its own body and
//! `getFilePointer()` answered a constant 0 there. Gated at the call site
//! below as of H8-1, 2026-08-20. Everything else on the two lists really is
//! disjoint — see the row comment for the full derivation.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::SeekFrom;

use parking_lot::Mutex;

use cratonvm_native_api::fd_table::FdId;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// Per-handle sync mode (O_SYNC / O_DSYNC)
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

/// The open file itself now lives in the global `fd_table` (so the RAF and
/// its `FileChannel` share one fd — see the module doc). The only per-handle
/// state the RAF natives still own is the requested sync mode, which `fd_table`
/// does not model. This side map is keyed by the **same** `FdId`, so it shares
/// the fd_table id namespace and carries no wrong-file hazard. Only "rws"/"rwd"
/// opens insert an entry; plain "r"/"rw" opens never touch it.
fn sync_modes() -> &'static Mutex<HashMap<FdId, SyncMode>> {
    static MAP: std::sync::OnceLock<Mutex<HashMap<FdId, SyncMode>>> = std::sync::OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run the fsync corresponding to `fd`'s sync mode, if any. No-op when the
/// handle was opened in a non-sync mode ("r" / "rw").
fn sync_if_needed(ctx: &dyn NativeContext, fd: FdId) -> Result<(), MethodCallFailed> {
    let mode = sync_modes().lock().get(&fd).copied();
    if let Some(mode) = mode {
        let data_only = matches!(mode, SyncMode::Data);
        ctx.fd_table().rw_sync(fd, data_only).map_err(io_err)?;
    }
    Ok(())
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
            RuntimeError::aioobe_index_only(if off < 0 { off } else { len }),
        )));
    }
    match (off as usize).checked_add(len as usize) {
        Some(end) if end <= arr_len => Ok(()),
        _ => Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::aioobe_index_only(off.saturating_add(len)),
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

/// Read the `fd_table` `FdId` from this RAF's `FileDescriptor`.  JDK 25
/// FileDescriptor has both `fd` (int) and `handle` (long on Windows); we
/// store the id in both. Valid `fd_table` ids are ≥ 3 (0/1/2 are reserved
/// stdio), so anything ≤ 2 (including the closed-file sentinel `-1` and the
/// unset `0`) reads back as "no open handle" — matching `fd_from_descriptor`
/// in `nio_native.rs`, so the RAF and its channel agree on what is open.
fn read_fd(ctx: &dyn NativeContext, this: ObjectRef) -> Option<FdId> {
    let fd_obj = raf_fd_object(ctx, this)?;
    match ctx.get_field_by_name(fd_obj, "fd") {
        Value::Int(v) if v > 2 => return Some(v as FdId),
        _ => {}
    }
    // Fallback to `handle` (long) — used by Windows JDK builds.
    match ctx.get_field_by_name(fd_obj, "handle") {
        Value::Long(v) if v > 2 && v < u32::MAX as i64 => Some(v as FdId),
        _ => None,
    }
}

fn write_handle(ctx: &mut dyn NativeContext, this: ObjectRef, fd: FdId) {
    // Real `RandomAccessFile.<init>` sets `this.fd = new FileDescriptor()` before
    // calling open0, but on CratonVM that constructor field-initializer does not
    // always land — `this.fd` reads back null (same class of issue as
    // java.net.Socket's null `socketLock`). With no FileDescriptor there is
    // nowhere to record the open handle, so `read_fd` returns None and every
    // length/read/seek behaves as a closed/empty file: `length()`=0, `read()`=-1,
    // and commons-compress's seek-from-EOF computes a negative offset
    // ("seek before beginning of file"). Create the FileDescriptor on demand.
    let fd_obj = match raf_fd_object(ctx, this) {
        Some(o) => o,
        None => match ctx.new_object("java/io/FileDescriptor") {
            Ok(Some(Value::Object(Some(fd_obj)))) => {
                ctx.set_field_by_name(this, "fd", Value::Object(Some(fd_obj)));
                fd_obj
            }
            _ => return,
        },
    };
    // Store the same id in both slots so code looking at either
    // gets a consistent value (and so `nio_native::fd_from_descriptor`,
    // which prefers `handle`, resolves the same fd_table entry).  JDK's
    // RandomAccessFile close path checks `fd.fd != -1`.
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd as i32));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(fd as i64));
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

    // O_RDONLY == 1 means read-only; O_RDWR == 2 means read+write.
    // Java spec: "rw" => O_RDWR; "r" => O_RDONLY.  The private open()
    // Java wrapper already translates the string mode into these bits.
    let write = mode_bits & O_RDWR != 0;

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

    // HotSpot's `handleOpen` refuses a directory (EISDIR) for RAF exactly as it
    // does for FileInputStream/FileOutputStream, so `new RandomAccessFile(dir,
    // "r")` throws `FileNotFoundException: <path> (Is a directory)` there.
    // Linux `open(2)` accepts a directory read-only, so the check has to be
    // explicit — see `reject_directory_open` for why it is per-call-site and
    // not in the fd table.
    crate::reject_directory_open(&path_str)?;

    // Open through the global fd_table (a FileReadWrite entry) so the RAF and
    // any FileChannel from raf.getChannel() resolve the SAME fd. Any open
    // error surfaces as FileNotFoundException, matching open0's declared throws.
    let fd = ctx
        .fd_table()
        .open_random_access(&path_str, write)
        .map_err(|_| fnf(&path_str))?;
    if let Some(mode) = sync_mode {
        sync_modes().lock().insert(fd, mode);
    }
    write_handle(ctx, this, fd);
    Ok(None)
}

fn native_read0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let fd = match read_fd(ctx, this) {
        Some(fd) => fd,
        None => return Ok(Some(Value::Int(-1))),
    };
    // fd_table().read_byte returns 0..255, or -1 at EOF — the exact
    // contract of RandomAccessFile.read0.
    match ctx.fd_table().read_byte(fd) {
        Ok(v) => Ok(Some(Value::Int(v))),
        Err(e) => Err(io_err(e)),
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
    let fd = match read_fd(ctx, this) {
        Some(fd) => fd,
        None => return Ok(Some(Value::Int(-1))),
    };
    let mut buf = vec![0u8; len];
    // Sequential read at the file's current cursor (advances it), matching
    // RandomAccessFile semantics.
    let n = match ctx.fd_table().rw_read(fd, &mut buf) {
        Ok(n) => n,
        Err(e) => return Err(io_err(e)),
    };
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    // AUDIT 2026-05-17: bulk write via NativeContext intrinsic.
    if !ctx.write_byte_array_from(arr, off, &buf[..n]) {
        return Err(io_err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "readBytes0: failed to copy bytes into Java array",
        )));
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
    let fd = match read_fd(ctx, this) {
        Some(fd) => fd,
        None => return Ok(None),
    };
    ctx.fd_table().write_byte(fd, byte).map_err(io_err)?;
    // Honour O_SYNC / O_DSYNC: per Java RandomAccessFile spec, "rws" and
    // "rwd" modes require that every write reach stable storage before the
    // call returns.
    sync_if_needed(ctx, fd)?;
    Ok(None)
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
    let copied = ctx.read_byte_array_into(arr, off, &mut buf);
    if copied != len {
        return Err(io_err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "writeBytes0: failed to copy full Java array range",
        )));
    }
    let fd = match read_fd(ctx, this) {
        Some(fd) => fd,
        None => return Ok(None),
    };
    // write_bytes on a FileReadWrite entry does write_all at the cursor.
    ctx.fd_table().write_bytes(fd, &buf).map_err(io_err)?;
    // Honour O_SYNC / O_DSYNC — see native_write0 above.
    sync_if_needed(ctx, fd)?;
    Ok(None)
}

#[allow(non_snake_case)]
fn native_getFilePointer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let fd = match read_fd(ctx, this) {
        Some(fd) => fd,
        None => return Ok(Some(Value::Long(0))),
    };
    match ctx.fd_table().rw_position(fd) {
        Ok(p) => Ok(Some(Value::Long(p as i64))),
        Err(e) => Err(io_err(e)),
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
    let fd = match read_fd(ctx, this) {
        Some(fd) => fd,
        None => return Ok(None),
    };
    match ctx.fd_table().rw_seek(fd, SeekFrom::Start(pos as u64)) {
        Ok(_) => Ok(None),
        Err(e) => Err(io_err(e)),
    }
}

fn native_length0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let fd = match read_fd(ctx, this) {
        Some(fd) => fd,
        None => return Ok(Some(Value::Long(0))),
    };
    match ctx.fd_table().file_size(fd) {
        Ok(n) => Ok(Some(Value::Long(n as i64))),
        Err(e) => Err(io_err(e)),
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
    let fd = match read_fd(ctx, this) {
        Some(fd) => fd,
        None => return Ok(None),
    };
    match ctx.fd_table().rw_set_length(fd, new_len as u64) {
        Ok(()) => Ok(None),
        Err(e) => Err(io_err(e)),
    }
}

fn native_close0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Some(fd) = read_fd(ctx, this) {
        // Drop any sync-mode bookkeeping, then close the fd_table entry
        // (which drops the File and closes the OS handle). Closing the RAF
        // closes its channel too — they share this one fd, matching HotSpot.
        sync_modes().lock().remove(&fd);
        let _ = ctx.fd_table().close(fd);
    }
    clear_handle(ctx, this);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

// JDK-ONLY-CLASSIFY: bridge — 10 of the 11 registrations here resolve to
// ACC_NATIVE methods on `java.io.RandomAccessFile` in JDK 25 (`initIDs`,
// `open0`, `read0`, `readBytes0`, `write0`, `writeBytes0`, `getFilePointer`,
// `seek0`, `length0`, `setLength0`). These are file-descriptor operations: the
// descriptor lives in this crate's fd table and there is no bytecode fallback
// in the image. Those ten now state `NativeKind::Bridge` at their own call
// sites rather than inheriting it from the scope below (L5, 2026-08-05).
//
// The eleventh is NOT one of them. `close0()V` is not declared by JDK 25's
// `RandomAccessFile` at all — schema-3 census `image_has_class: true,
// declared: false`, confirmed with `javap -p java.io.RandomAccessFile`, which
// closes through `FileCleanable`/`fd` and has `close()` as ordinary bytecode.
// A registration that targets nothing on this image is not an adjudicated
// bridge, so it keeps the ambient category and the `set_category` scope stays
// for it. (The earlier "8 of the 11" and the names `readBytes`, `length`,
// `setLength` in this marker came from a static read of pre-JDK-19 spellings;
// the census names the descriptors this crate actually registers.) Residuals:
// l5-native-io-bridge-residuals-RETIRED-20260810.md
//
// COUNT CAVEAT (H8-1, 2026-08-20): the counts above describe the DEFAULT arm.
// `getFilePointer` is now gated on `crate::real_raf_enabled()`, so under
// `CRATONVM_SYNTHETIC_RAF=1` this registrar contributes one row fewer and the
// synthetic twin in `lib.rs::register_io_extras_natives` owns that triple
// instead. See the row's own comment for why.
pub fn register_random_access_file_natives(registry: &mut NativeMethodRegistry) {
    use cratonvm_native_api::NativeKind;
    let __prev_cat = registry.current_category();
    registry.set_category(NativeKind::Bridge);
    let raf = "java/io/RandomAccessFile";
    registry.register_with_kind(raf, "initIDs", "()V", native_initIDs, NativeKind::Bridge);
    registry.register_with_kind(
        raf,
        "open0",
        "(Ljava/lang/String;I)V",
        native_open0,
        NativeKind::Bridge,
    );
    registry.register_with_kind(raf, "read0", "()I", native_read0, NativeKind::Bridge);
    registry.register_with_kind(
        raf,
        "readBytes0",
        "([BII)I",
        native_readBytes0,
        NativeKind::Bridge,
    );
    registry.register_with_kind(raf, "write0", "(I)V", native_write0, NativeKind::Bridge);
    registry.register_with_kind(
        raf,
        "writeBytes0",
        "([BII)V",
        native_writeBytes0,
        NativeKind::Bridge,
    );
    // THE ONE ROW ON THIS REGISTRAR THAT THE SYNTHETIC-RAF GATE HAS TO REACH.
    //
    // `getFilePointer()J` is the only method that is both (a) `public native`
    // on JDK 25 — `javap -p -s java.io.RandomAccessFile` on
    // `jdk-25.0.3.9-hotspot` shows `public native long getFilePointer()`, the
    // sole ACC_NATIVE method of the ten here that is not `private` — and
    // (b) part of the PUBLIC surface that `register_io_extras_natives`
    // re-implements inside its `if !real_raf_enabled()` block. It is the whole
    // intersection of the two lists: the other nine here are `open0`, `read0`,
    // `readBytes0`, `write0`, `writeBytes0`, `seek0`, `length0`, `setLength0`,
    // `initIDs`, none of which the synthetic block names.
    //
    // Until 2026-08-20 this row was UNCONDITIONAL while its twin was gated,
    // and `register_random_access_file_natives` runs six lines AFTER
    // `register_io_extras_natives` in `register_io_natives`, so this row won
    // in BOTH settings of the flag and `CRATONVM_SYNTHETIC_RAF=1` could not
    // reach `getFilePointer` at all. That is not a harmless overlap: the two
    // bodies read DIFFERENT layouts. `native_getFilePointer` below goes
    // through `read_fd`, i.e. `this.fd` as a `FileDescriptor` OBJECT; the
    // synthetic `native_raf_init` writes an `Int` fd into slot 0 and no
    // `FileDescriptor` at all. So in the synthetic arm `raf_fd_object`
    // answered `None` and `getFilePointer()` returned a constant **0** —
    // silently, for every RAF — while `seek`/`length`/`read` all worked off
    // the synthetic layout. `[flag != mode drops it]`.
    //
    // Gated here rather than un-gating the synthetic twin: `getFilePointer` is
    // genuinely ACC_NATIVE, so in the DEFAULT (real-RAF) arm this Bridge is
    // the only implementation there is and must stay. H8-1, 2026-08-20.
    if crate::real_raf_enabled() {
        registry.register_with_kind(
            raf,
            "getFilePointer",
            "()J",
            native_getFilePointer,
            NativeKind::Bridge,
        );
    }
    registry.register_with_kind(raf, "seek0", "(J)V", native_seek0, NativeKind::Bridge);
    registry.register_with_kind(raf, "length0", "()J", native_length0, NativeKind::Bridge);
    registry.register_with_kind(
        raf,
        "setLength0",
        "(J)V",
        native_setLength0,
        NativeKind::Bridge,
    );
    // Not ACC_NATIVE — not declared by JDK 25's RandomAccessFile at all.
    // Left on the ambient category deliberately; see the marker above.
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_native_api::fd_table::FileDescriptorTable;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::io::{Read, Write};

    /// The crux of the TC0622 fix: a file opened via `open_random_access`
    /// (the path RAF.open0 now takes) is resolvable by BOTH the sequential
    /// RAF read path (`rw_read`) AND the nio channel size path (`file_size`)
    /// through the SAME fd. Previously RAF used a private ≥1000 handle map
    /// that `file_size` could not see → `IOException: size0: bad fd for size`.
    #[test]
    fn open_random_access_shares_fd_for_read_and_size() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"hello world").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let table = FileDescriptorTable::new();
        let fd = table.open_random_access(&path, false).unwrap();

        // nio FileChannel.size() → FileDispatcherImpl.size0 → file_size
        assert_eq!(table.file_size(fd).unwrap(), 11);

        // RAF.readBytes0 → rw_read at the cursor
        let mut buf = [0u8; 5];
        let n = table.rw_read(fd, &mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b"hello");

        // ...and size0 still works after the cursor moved (it saves/restores).
        assert_eq!(table.file_size(fd).unwrap(), 11);

        table.close(fd).unwrap();
    }

    #[test]
    fn open_random_access_seek_and_length() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"0123456789").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let table = FileDescriptorTable::new();
        let fd = table.open_random_access(&path, false).unwrap();

        assert_eq!(table.file_size(fd).unwrap(), 10);

        table.rw_seek(fd, SeekFrom::Start(3)).unwrap();
        assert_eq!(table.rw_position(fd).unwrap(), 3);
        let mut buf = [0u8; 3];
        let n = table.rw_read(fd, &mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf, b"345");

        table.close(fd).unwrap();
    }

    /// "rw" mode opens read+write+create; write-then-read-back roundtrips,
    /// and the durable-sync path (`rw_sync`, used by "rws"/"rwd") succeeds.
    #[test]
    fn open_random_access_write_and_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raf_rw.bin");
        let path_s = path.to_str().unwrap().to_string();

        let table = FileDescriptorTable::new();
        let fd = table.open_random_access(&path_s, true).unwrap();
        table.write_bytes(fd, b"durable").unwrap();
        // "rwd"/"rws" per-write durability hook.
        table.rw_sync(fd, true).unwrap();
        table.rw_sync(fd, false).unwrap();
        table.close(fd).unwrap();

        let mut s = String::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "durable");
    }

    /// "r" mode must open the underlying file read-only: a write fails at
    /// the OS level (justifying why RAF "r" rejects writes).
    #[test]
    fn raf_native_read_write_bytes_roundtrip_nonzero_payload() {
        use crate::test_support::MockNativeContext;
        use cratonvm_native_api::NativeContext;
        use cratonvm_types::ArrayElementType;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raf-bytes.bin");
        let path_s = path.to_str().unwrap().to_string();

        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(0);
        let fd_obj = ctx.alloc_object(0);
        ctx.set_field_by_name(this, "fd", Value::Object(Some(fd_obj)));
        let path_obj = ctx.attach_string(&path_s);

        native_open0(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(path_obj)),
                Value::Int(O_RDWR),
            ],
        )
        .unwrap();

        let src = ctx.new_array(ArrayElementType::Byte, 4);
        for (i, b) in [0x11_i32, 0x22, 0x33, 0x44].into_iter().enumerate() {
            ctx.set_array_element(src, i, Value::Int(b));
        }
        native_writeBytes0(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(src)),
                Value::Int(0),
                Value::Int(4),
            ],
        )
        .unwrap();
        native_seek0(&mut ctx, &[Value::Object(Some(this)), Value::Long(0)]).unwrap();

        let dst = ctx.new_array(ArrayElementType::Byte, 4);
        let n = native_readBytes0(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(dst)),
                Value::Int(0),
                Value::Int(4),
            ],
        )
        .unwrap();
        assert_eq!(n, Some(Value::Int(4)));
        for (i, b) in [0x11_i32, 0x22, 0x33, 0x44].into_iter().enumerate() {
            assert_eq!(ctx.get_array_element(dst, i), Value::Int(b));
        }

        native_close0(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    }

    #[test]
    fn open_options_read_only_rejects_write() {
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
