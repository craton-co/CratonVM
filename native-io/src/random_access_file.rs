// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
/// does not model. This side map is keyed by the **same** `FdId`. Only
/// "rws"/"rwd" opens insert an entry; plain "r"/"rw" opens remove any row left
/// under their id.
///
/// JIT r23 wave 3 review (`r23-w3-review-fd-os-keys-reuse-audit`): each row
/// carries the fd's `FileDescriptorTable::generation` at open. The row is
/// dropped by `native_close0` only; a close that reaches the fd another way
/// (the JDK's `FileDescriptor.close0` / `closeAll`, the shared `FileChannel`)
/// leaves it behind, and under `CRATONVM_FD_TABLE_OS_KEYS` the OS reuses the
/// value. A row whose generation is not the live entry's is stale and ignored
/// (and dropped), so a later file on the reused value never inherits the sync.
fn sync_modes() -> &'static Mutex<HashMap<FdId, (SyncMode, u64)>> {
    static MAP: std::sync::OnceLock<Mutex<HashMap<FdId, (SyncMode, u64)>>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `fd`'s sync mode, if its row belongs to the live entry (see `sync_modes`).
fn live_sync_mode(ctx: &dyn NativeContext, fd: FdId) -> Option<SyncMode> {
    let mut map = sync_modes().lock();
    let (mode, generation) = map.get(&fd).copied()?;
    if ctx.fd_table().generation(fd) == Some(generation) {
        return Some(mode);
    }
    map.remove(&fd);
    None
}

/// Run the fsync corresponding to `fd`'s sync mode, if any. No-op when the
/// handle was opened in a non-sync mode ("r" / "rw").
fn sync_if_needed(ctx: &dyn NativeContext, fd: FdId) -> Result<(), MethodCallFailed> {
    let mode = live_sync_mode(ctx, fd);
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
    // HotSpot's message is `strerror(errno)` alone; Rust's `Display` for an OS
    // error appends " (os error N)".
    let mut message = e.to_string();
    if e.raw_os_error().is_some() {
        if let Some(cut) = message.rfind(" (os error ") {
            message.truncate(cut);
        }
    }
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException { message }))
}

/// HotSpot's answer for any `RandomAccessFile` operation on a closed (or
/// never-opened) descriptor: `io_util.c` / `RandomAccessFile.c` raise
/// `IOException("Stream Closed")` instead of reading as end-of-file or
/// silently doing nothing.
fn stream_closed() -> MethodCallFailed {
    io_err(std::io::Error::other("Stream Closed"))
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
    // HotSpot (`io_util.c` `readBytes`/`writeBytes`) raises a plain
    // `IndexOutOfBoundsException` with no message, not the
    // `ArrayIndexOutOfBoundsException` of an array access.
    let out_of_bounds = || {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IndexOutOfBoundsException {
            message: None,
        }))
    };
    if off < 0 || len < 0 {
        return Err(out_of_bounds());
    }
    match (off as usize).checked_add(len as usize) {
        Some(end) if end <= arr_len => Ok(()),
        _ => Err(out_of_bounds()),
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
/// store the id in the platform's one (`CRATONVM_FD_FIELDS_PER_PLATFORM`,
/// both with it off), so this reads `fd` and then `handle`. Valid `fd_table` ids are ≥ 3 (0/1/2 are reserved
/// stdio), so anything ≤ 2 (including the closed-file sentinel `-1` and the
/// unset `0`) reads back as "no open handle" — matching `fd_from_descriptor`
/// in `nio_native.rs`, so the RAF and its channel agree on what is open.
fn read_fd(ctx: &dyn NativeContext, this: ObjectRef) -> Option<FdId> {
    // SBR5 lane ncall: the two by-name field reads (`RandomAccessFile.fd`,
    // `FileDescriptor.fd`/`handle`) were ~60 ns of every seek/read/write. The
    // slots are resolved once per (VM, receiver class, descriptor class) and
    // read by index afterwards. The class ids come from the forwarded objects
    // (`class_id_of_object_forwarded`), so a slot is never paired with another
    // object's layout. Anything the memo does not recognise takes the by-name
    // path below, which refills it.
    let vm = ctx.vm_identity();
    let raf_cid = ctx.class_id_of_object_forwarded(this).as_u32();
    let memo = FD_SLOTS.with(|c| c.get());
    if memo.vm == vm && memo.raf_cid == raf_cid && raf_cid != 0 {
        if let Value::Object(Some(fd_obj)) = ctx.get_field(this, memo.raf_fd) {
            if ctx.class_id_of_object_forwarded(fd_obj).as_u32() == memo.fdo_cid {
                // t1 wave 8, lane I71 (page
                // `t1-L71-jar-get-entry-bimodal-and-define-native-cost` §3):
                // on Windows the id lives in `handle` (`fd` is -1 under
                // `CRATONVM_FD_FIELDS_PER_PLATFORM`, default on; both hold the
                // same id with it off), so ask `handle` first and skip the
                // `fd` read every seek/read paid. Same answer in every shape
                // the VM writes; `descriptor_fd_id` already reads in this order.
                if cfg!(windows) {
                    if let Value::Long(v) = ctx.get_field(fd_obj, memo.fdo_handle) {
                        if v > 2 && v < u32::MAX as i64 {
                            return Some(v as FdId);
                        }
                    }
                }
                if let Value::Int(v) = ctx.get_field(fd_obj, memo.fdo_fd) {
                    if v > 2 {
                        return Some(v as FdId);
                    }
                }
                if cfg!(windows) {
                    return None;
                }
                return match ctx.get_field(fd_obj, memo.fdo_handle) {
                    Value::Long(v) if v > 2 && v < u32::MAX as i64 => Some(v as FdId),
                    _ => None,
                };
            }
        }
    }
    let fd = read_fd_by_name(ctx, this);
    if fd.is_some() && raf_cid != 0 {
        fill_fd_slots(ctx, this, vm, raf_cid);
    }
    fd
}

/// Per-thread memo of the field slots [`read_fd`] reads.
#[derive(Clone, Copy)]
struct FdSlots {
    vm: usize,
    raf_cid: u32,
    raf_fd: usize,
    fdo_cid: u32,
    fdo_fd: usize,
    fdo_handle: usize,
}

thread_local! {
    static FD_SLOTS: std::cell::Cell<FdSlots> = const {
        std::cell::Cell::new(FdSlots {
            vm: 0,
            raf_cid: 0,
            raf_fd: 0,
            fdo_cid: 0,
            fdo_fd: 0,
            fdo_handle: 0,
        })
    };
}

#[cold]
#[inline(never)]
fn fill_fd_slots(ctx: &dyn NativeContext, this: ObjectRef, vm: usize, raf_cid: u32) {
    let Value::Object(Some(fd_obj)) = ctx.get_field_by_name(this, "fd") else {
        return;
    };
    let fdo_cid = ctx.class_id_of_object_forwarded(fd_obj);
    let raf_class = ctx.class_id_of_object_forwarded(this);
    if fdo_cid.as_u32() == 0 || raf_class.as_u32() != raf_cid {
        return;
    }
    let (Some(raf_fd), Some(fdo_fd), Some(fdo_handle)) = (
        ctx.resolve_field_index_by_class_id(raf_class, "fd"),
        ctx.resolve_field_index_by_class_id(fdo_cid, "fd"),
        ctx.resolve_field_index_by_class_id(fdo_cid, "handle"),
    ) else {
        return;
    };
    FD_SLOTS.with(|c| {
        c.set(FdSlots {
            vm,
            raf_cid,
            raf_fd,
            fdo_cid: fdo_cid.as_u32(),
            fdo_fd,
            fdo_handle,
        })
    });
}

fn read_fd_by_name(ctx: &dyn NativeContext, this: ObjectRef) -> Option<FdId> {
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
    // GC: `new_object` allocates and the store that follows goes through
    // `this`. Not reported by the audit's parameter rule — the allocation and
    // the stale store share one `let … match` statement, and the rule asks
    // whether a use comes after the statement the call is in.
    let pin = ctx.pin_native_root(this);
    let fd_obj = match raf_fd_object(ctx, this) {
        Some(o) => o,
        None => match ctx.new_object("java/io/FileDescriptor") {
            Ok(Some(Value::Object(Some(fd_obj)))) => {
                let this = ctx.read_native_pin(pin, this);
                ctx.set_field_by_name(this, "fd", Value::Object(Some(fd_obj)));
                fd_obj
            }
            _ => {
                ctx.unpin_native_roots(pin);
                return;
            }
        },
    };
    ctx.unpin_native_roots(pin);
    // The platform's spelling (`CRATONVM_FD_FIELDS_PER_PLATFORM`, JIT r23 w1
    // L6): Unix `fd = id, handle = -1`, Windows `fd = -1, handle = id`, as
    // HotSpot's `RandomAccessFile` descriptors read. `read_fd` and
    // `nio_native::descriptor_fd_id` find the id in either field; with the
    // switch off both fields hold it, as before.
    crate::nio_native::store_descriptor_id(ctx, fd_obj, fd);
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
        // JIT r23 wave 5, lane L3 (`CRATONVM_IO_WINDOWS_OS_ERROR_REASON`): the
        // JDK opens "" on Linux and reports ENOENT, `" (No such file or
        // directory)"`; Windows' `pathToNTPath` refuses it first (bare "").
        if !cfg!(windows) {
            if let Some(reason) =
                crate::os_error_reason(&std::io::Error::from_raw_os_error(2))
            {
                return Err(fnf(&format!(" ({reason})")));
            }
        }
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
    let may_wait = crate::reject_directory_open_may_wait(&path_str)?;

    // Open through the global fd_table (a FileReadWrite entry) so the RAF and
    // any FileChannel from raf.getChannel() resolve the SAME fd. Any open
    // error surfaces as FileNotFoundException, matching open0's declared throws.
    // The i53 residual: a FIFO's open waits for its writer, in a blocking
    // region (`open_waiting_on_a_peer`).
    let mut refs = [Value::Object(Some(this))];
    let fd = crate::open_waiting_on_a_peer(ctx, may_wait, &mut refs, |ctx| {
        ctx.fd_table().open_random_access(&path_str, write)
    })
    // JIT r23 wave 5, lane L3 (`CRATONVM_IO_WINDOWS_OS_ERROR_REASON`): on
    // every platform the reason HotSpot prints, `<path> (<OS text>)`
    // (`strerror` on Linux, the system text on Windows); the bare path only
    // for an error with no OS code or with the switch off.
    .map_err(|e| match crate::os_error_reason(&e) {
        Some(_) => crate::file_not_found_because(&path_str, &e),
        None => fnf(&path_str),
    })?;
    let Value::Object(Some(this)) = refs[0] else {
        return Ok(None);
    };
    match sync_mode {
        Some(mode) => {
            let generation = ctx.fd_table().generation(fd).unwrap_or(0);
            sync_modes().lock().insert(fd, (mode, generation));
        }
        // A row an earlier owner of this (reused) id left behind.
        None => {
            sync_modes().lock().remove(&fd);
        }
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
        None => return Err(stream_closed()),
    };
    if waits_on_a_peer(ctx, fd) {
        if ctx.decline_leaf_call() {
            return Ok(None);
        }
        return peer_wait_read0(ctx, fd);
    }
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
    // A null buffer: `io_util.c`'s `JNU_ThrowNullPointerException(env, NULL)`,
    // an NPE with no message, before the bounds and the descriptor (interpreter
    // round i5 wave 100, lane L7; `L7W100ShadowIndexTexts` rows `raf-read-null`,
    // `raf-readFully-null`).
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
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
        None => return Err(stream_closed()),
    };
    if waits_on_a_peer(ctx, fd) {
        if ctx.decline_leaf_call() {
            return Ok(None);
        }
        return peer_wait_read_bytes0(ctx, args, fd, off, len);
    }
    // Sequential read at the file's current cursor (advances it), matching
    // RandomAccessFile semantics.
    with_io_scratch(len, |buf| {
        let n = match ctx.fd_table().rw_read(fd, buf) {
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
    })
}

/// The i53 residual: does a read or write of `fd` possibly wait on a peer
/// (a FIFO, a tty, a device -- anything but a regular file)? Such a call
/// must not hold every collection while it waits: HotSpot's thread is in
/// native there, which a safepoint does not wait for. The data rows are
/// LEAF natives, which may not enter a blocking region, so on the leaf path
/// they decline (`NativeContext::decline_leaf_call`, before any side effect)
/// and the funnel runs them again into the `peer_wait_*` helpers below,
/// which wait inside one (`FileInputStream.read0` / `readBytes`'s split in
/// `lib.rs`). A regular file keeps the leaf fast path.
fn waits_on_a_peer(ctx: &dyn NativeContext, fd: FdId) -> bool {
    !ctx.fd_table().is_regular_file(fd)
}

/// `read0` of a descriptor that may wait on a peer, in a blocking region
/// (full funnel only; see [`waits_on_a_peer`]).
fn peer_wait_read0(ctx: &mut dyn NativeContext, fd: FdId) -> MethodCallResult {
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    let result = ctx.fd_table().read_byte(fd);
    ctx.end_blocking_region();
    match result {
        Ok(v) => Ok(Some(Value::Int(v))),
        Err(e) => Err(io_err(e)),
    }
}

/// `readBytes0` of a descriptor that may wait on a peer, in a blocking
/// region (full funnel only; see [`waits_on_a_peer`]). The array is re-read
/// from the arguments the region's end refreshed: a collection may have
/// moved it meanwhile.
fn peer_wait_read_bytes0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    fd: FdId,
    off: usize,
    len: usize,
) -> MethodCallResult {
    let mut buf = vec![0u8; len];
    let mut largs = args.to_vec();
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    let result = ctx.fd_table().rw_read(fd, &mut buf);
    ctx.end_blocking_region_refs(&mut largs);
    let n = result.map_err(io_err)?;
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    let Some(Value::Object(Some(arr))) = largs.get(1).copied() else {
        return Ok(Some(Value::Int(-1)));
    };
    if !ctx.write_byte_array_from(arr, off, &buf[..n]) {
        return Err(io_err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "readBytes0: failed to copy bytes into Java array",
        )));
    }
    Ok(Some(Value::Int(n as i32)))
}

/// `write0` / `writeBytes0` of a descriptor that may wait on a peer, in a
/// blocking region (full funnel only; see [`waits_on_a_peer`]). `bytes` was
/// copied out of the Java array before the region.
fn peer_wait_write(
    ctx: &mut dyn NativeContext,
    fd: FdId,
    bytes: &[u8],
) -> Result<(), MethodCallFailed> {
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    let result = ctx.fd_table().write_bytes(fd, bytes);
    ctx.end_blocking_region();
    result.map_err(io_err)
}

/// Run `f` over a `len`-byte scratch buffer. A per-thread buffer up to
/// [`IO_SCRATCH_MAX`] is reused across calls (a zeroed `vec![0u8; len]` per
/// 1 KB read/write was ~5% of the call); anything larger, or a nested use,
/// takes a fresh vector. The contents on entry are unspecified.
fn with_io_scratch<R>(len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    const IO_SCRATCH_MAX: usize = 64 * 1024;
    thread_local! {
        static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    if len <= IO_SCRATCH_MAX {
        let mut f = Some(f);
        let served = SCRATCH.with(|cell| {
            let mut v = cell.try_borrow_mut().ok()?;
            if v.len() < len {
                v.resize(len, 0);
            }
            let f = f.take()?;
            Some(f(&mut v[..len]))
        });
        if let Some(r) = served {
            return r;
        }
        // `f` is only taken once the buffer is in hand, so a failed borrow
        // leaves it for the fallback.
        if let Some(f) = f {
            let mut buf = vec![0u8; len];
            return f(&mut buf);
        }
        unreachable!("scratch closure taken but no result produced");
    }
    let mut buf = vec![0u8; len];
    f(&mut buf)
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
        None => return Err(stream_closed()),
    };
    if waits_on_a_peer(ctx, fd) {
        if ctx.decline_leaf_call() {
            return Ok(None);
        }
        peer_wait_write(ctx, fd, &[byte])?;
        sync_if_needed(ctx, fd)?;
        return Ok(None);
    }
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
    // A null buffer: `io_util.c`'s `JNU_ThrowNullPointerException(env, NULL)`,
    // an NPE with no message, before the bounds and the descriptor (interpreter
    // round i5 wave 100, lane L7; `L7W100ShadowIndexTexts` rows `raf-read-null`,
    // `raf-readFully-null`).
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
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
    let fd = read_fd(ctx, this);
    if let Some(fd) = fd {
        if waits_on_a_peer(ctx, fd) {
            if ctx.decline_leaf_call() {
                return Ok(None);
            }
            let mut buf = vec![0u8; len];
            if ctx.read_byte_array_into(arr, off, &mut buf) != len {
                return Err(io_err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "writeBytes0: failed to copy full Java array range",
                )));
            }
            peer_wait_write(ctx, fd, &buf)?;
            sync_if_needed(ctx, fd)?;
            return Ok(None);
        }
    }
    with_io_scratch(len, |buf| {
        // AUDIT 2026-05-17: bulk read via NativeContext intrinsic.
        let copied = ctx.read_byte_array_into(arr, off, buf);
        if copied != len {
            return Err(io_err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "writeBytes0: failed to copy full Java array range",
            )));
        }
        let fd = match fd {
            Some(fd) => fd,
            None => return Err(stream_closed()),
        };
        // write_bytes on a FileReadWrite entry does write_all at the cursor.
        ctx.fd_table().write_bytes(fd, buf).map_err(io_err)?;
        // Honour O_SYNC / O_DSYNC — see native_write0 above.
        sync_if_needed(ctx, fd)?;
        Ok(None)
    })
}

#[allow(non_snake_case)]
fn native_getFilePointer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let fd = match read_fd(ctx, this) {
        Some(fd) => fd,
        None => return Err(stream_closed()),
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
        None => return Err(stream_closed()),
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
        None => return Err(stream_closed()),
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
        None => return Err(stream_closed()),
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
            let raf = "java/io/RandomAccessFile";
    registry.register(raf, "initIDs", "()V", native_initIDs, NativeKind::Bridge);
    registry.register(
        raf,
        "open0",
        "(Ljava/lang/String;I)V",
        native_open0,
        NativeKind::Bridge,
    );
    // SBR5 lane ncall: the data-path rows below are LEAF natives
    // (`NativeMethodRegistry::set_leaf`): a field read or two, the fd-table
    // lock, one syscall and a bulk copy into the caller's array. None
    // allocates on the Java heap, parks or re-enters Java; an error is a
    // returned `Err`. Their ~1 us each on the full funnel was the cost of a
    // 1 KB `RandomAccessFile` read/write against HotSpot's ~0.5 us. `open0`,
    // `setLength0` and `close0` stay on the funnel (they allocate a
    // `FileDescriptor` or touch the sync-mode map).
    let __prev_leaf = registry.current_leaf();
    registry.set_leaf(true);
    registry.register(raf, "read0", "()I", native_read0, NativeKind::Bridge);
    registry.register(
        raf,
        "readBytes0",
        "([BII)I",
        native_readBytes0,
        NativeKind::Bridge,
    );
    registry.register(raf, "write0", "(I)V", native_write0, NativeKind::Bridge);
    registry.register(
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
        registry.register(
            raf,
            "getFilePointer",
            "()J",
            native_getFilePointer,
            NativeKind::Bridge,
        );
    }
    registry.register(raf, "seek0", "(J)V", native_seek0, NativeKind::Bridge);
    registry.register(raf, "length0", "()J", native_length0, NativeKind::Bridge);
    registry.set_leaf(__prev_leaf);
    registry.register(
        raf,
        "setLength0",
        "(J)V",
        native_setLength0,
        NativeKind::Bridge,
    );
    // Not ACC_NATIVE — not declared by JDK 25's RandomAccessFile at all.
    // Left on the ambient category deliberately; see the marker above.
    }

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
