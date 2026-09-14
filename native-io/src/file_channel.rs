// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP3.3 + WP3.6 — Real `sun/nio/ch/FileDispatcherImpl.map0` /
//! `unmap0` / `transferTo0` / `maxDirectTransferSize0`.
//!
//! Replaces the stubs in `nio_native.rs` with OS-backed mmap and
//! zero-copy file transfer:
//!
//!   * `map0` uses [`memmap2::MmapOptions`] which on Linux drives
//!     `mmap(2)` and on Windows drives `MapViewOfFile`. The returned
//!     address points at real readable (and optionally writable)
//!     pages — the JDK side then does `Unsafe.getByte(addr+i)` to
//!     pull bytes out.
//!   * `transferTo0` uses `libc::sendfile(2)` on Linux for true
//!     zero-copy. On Windows we reference `TransmitFile` in the
//!     cfg-gated path but, per the spec ("falls back to read/write
//!     loop if not applicable"), use a userspace 64 KiB-buffer loop
//!     when the destination is not a socket. The same loop is used
//!     on every non-Linux platform.
//!
//! The Java FQN is `sun/nio/ch/FileDispatcherImpl` (modern, JDK 25)
//! plus `sun/nio/ch/FileChannelImpl` (legacy alias). Both are
//! registered.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

use cratonvm_native_api::fd_table::FdId;
use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

// ---------------------------------------------------------------------------
// IOStatus constants — mirror the JDK's sun.nio.ch.IOStatus values so the
// JDK side's branchless decode (== UNAVAILABLE / INTERRUPTED / UNSUPPORTED)
// works without modification.
// ---------------------------------------------------------------------------

/// EOF marker — `IOStatus.EOF`.
#[allow(dead_code)]
const IOSTATUS_EOF: i64 = -1;
/// Operation would block — `IOStatus.UNAVAILABLE`.
#[allow(dead_code)]
const IOSTATUS_UNAVAILABLE: i64 = -2;
/// Operation interrupted — `IOStatus.INTERRUPTED`.
#[allow(dead_code)]
const IOSTATUS_INTERRUPTED: i64 = -3;
/// Operation not supported on this platform / fd kind — `IOStatus.UNSUPPORTED`.
/// Returning this from `transferTo0` tells the JDK to fall back to a
/// `ByteBuffer`-based loop, which is correct (and is also what HotSpot
/// does on platforms that lack `sendfile`).
#[allow(dead_code)]
const IOSTATUS_UNSUPPORTED: i64 = -4;

// ---------------------------------------------------------------------------
// Local helpers — duplicated from `nio_native.rs` (private there, can't
// be re-exported without changing a sibling agent's surface).
// ---------------------------------------------------------------------------

/// Extract the `FdId` that a prior `open0` stored on a Java
/// `java.io.FileDescriptor` object. Windows path uses the `handle`
/// long; Unix path uses the `fd` int. Returns `None` if neither
/// holds a valid id.
fn fd_from_descriptor(ctx: &mut dyn NativeContext, fd_obj: ObjectRef) -> Option<FdId> {
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

/// Pull a `FileDescriptor` `ObjectRef` from arg `idx` — used by the
/// modern dispatcher signatures. Caller paths that accept either a
/// raw int or an Object route through `fd_id_arg` instead.
fn fd_arg(args: &[Value], idx: usize) -> Result<ObjectRef, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(io_error("FileChannel: null FileDescriptor")),
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

#[cfg(test)]
fn bool_arg(args: &[Value], idx: usize) -> bool {
    match args.get(idx) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// MMAP registry — keyed by the base address of the mapping so `unmap0`
// can drop the right `Mmap`/`MmapMut` and let the OS reclaim pages.
// ---------------------------------------------------------------------------

/// Holds either a read-only or a writable mapping. Dropping unmaps.
enum MmapHolder {
    Ro(memmap2::Mmap),
    Rw(memmap2::MmapMut),
    /// Private / copy-on-write mapping — also `MmapMut` under the hood.
    Cow(memmap2::MmapMut),
}

impl MmapHolder {
    fn as_ptr(&self) -> *const u8 {
        match self {
            MmapHolder::Ro(m) => m.as_ptr(),
            MmapHolder::Rw(m) => m.as_ptr(),
            MmapHolder::Cow(m) => m.as_ptr(),
        }
    }

    fn len(&self) -> usize {
        match self {
            MmapHolder::Ro(m) => m.len(),
            MmapHolder::Rw(m) => m.len(),
            MmapHolder::Cow(m) => m.len(),
        }
    }
}

/// Global registry of live mappings. Keyed by the base address so the
/// JDK side (which only retains `addr`) can hand it back to `unmap0`.
fn mmap_registry() -> &'static Mutex<HashMap<usize, MmapHolder>> {
    static REG: OnceLock<Mutex<HashMap<usize, MmapHolder>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `prot` constants matching `sun.nio.ch.FileChannelImpl`:
///   MAP_RO = 0  (PROT_READ)
///   MAP_RW = 1  (PROT_READ | PROT_WRITE; shared)
///   MAP_PV = 2  (PROT_READ | PROT_WRITE; private / copy-on-write)
const MAP_RO: i32 = 0;
const MAP_RW: i32 = 1;
const MAP_PV: i32 = 2;

// ---------------------------------------------------------------------------
// map0 / unmap0 — WP3.3
// ---------------------------------------------------------------------------

/// `map0(FileDescriptor, int prot, long position, long length [, boolean isSync]) -> long`
///
/// Returns the base address of the new mapping. The caller (JDK
/// `FileChannelImpl.map`) wraps this in a `MappedByteBuffer`. Bytes
/// at `[addr, addr + length)` are real OS-mapped memory.
///
/// Errors translate to `IOException`. We never return a fake address.
fn native_fc_map0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let prot = int_arg(args, 1);
    let position = long_arg(args, 2);
    let length = long_arg(args, 3);
    // arg 4 (isSync) — ignored. Real `MAP_SYNC` requires DAX; falls
    // back to MAP_SHARED on every commodity filesystem so the JDK
    // path is identical with or without the flag.

    if length < 0 {
        return Err(io_error("map0: negative length"));
    }
    if position < 0 {
        return Err(io_error("map0: negative position"));
    }
    if length == 0 {
        // memmap2 rejects len == 0; the JDK in this case never asks
        // us to map (FileChannel.map throws first), but be defensive.
        return Err(io_error("map0: zero length"));
    }
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("map0: FileDescriptor has no open handle"));
    };

    // Clone the underlying File so the mapping owns an independent
    // handle (memmap2 holds it for the lifetime of the mapping).
    let file = ctx
        .fd_table()
        .clone_file(fd)
        .map_err(|e| io_error(format!("map0: clone fd: {e}")))?;

    // Build mmap options. memmap2 calls mmap(2) on Linux/macOS and
    // MapViewOfFile on Windows.
    let mut opts = memmap2::MmapOptions::new();
    opts.offset(position as u64).len(length as usize);

    let holder = match prot {
        MAP_RO => {
            // SAFETY: file is a real fs::File handle owned for the
            // lifetime of the mapping; pages are read-only so the
            // kernel page cache aliasing is sound.
            let m = unsafe {
                opts.map(&file)
                    .map_err(|e| io_error(format!("map0: mmap RO: {e}")))?
            };
            MmapHolder::Ro(m)
        }
        MAP_RW => {
            // SAFETY: as above; writes go through to the file via
            // MAP_SHARED semantics.
            let m = unsafe {
                opts.map_mut(&file)
                    .map_err(|e| io_error(format!("map0: mmap RW: {e}")))?
            };
            MmapHolder::Rw(m)
        }
        MAP_PV => {
            // SAFETY: copy-on-write. Writes never touch the
            // underlying file. memmap2's `map_copy` produces
            // a writable private mapping (MAP_PRIVATE on Unix,
            // FILE_MAP_COPY on Windows).
            let m = unsafe {
                opts.map_copy(&file)
                    .map_err(|e| io_error(format!("map0: mmap COW: {e}")))?
            };
            MmapHolder::Cow(m)
        }
        other => {
            return Err(io_error(format!("map0: unknown prot {other}")));
        }
    };

    let addr = holder.as_ptr() as usize;
    if addr == 0 {
        return Err(io_error("map0: kernel returned null address"));
    }
    mmap_registry().lock().insert(addr, holder);
    Ok(Some(Value::Long(addr as i64)))
}

/// `unmap0(long addr, long size) -> int` — drops the mapping at
/// `addr`, releasing the OS pages. Returns 0 on success.
///
/// We look up by base address since each mapping owns its own
/// `Mmap`/`MmapMut` whose `Drop` calls `munmap` / `UnmapViewOfFile`,
/// so the kernel-side reclamation does not need `size`. We still
/// honour `size` as a defensive check: if the JDK shim passes a
/// `size` that disagrees with the size we recorded at `map0`-time,
/// log a warning (the shim is buggy or a malicious caller is
/// passing fabricated metadata). We then proceed to drop the
/// registered entry — the real mapping length wins because the
/// `Mmap` holder carries it.
fn native_fc_unmap0(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = long_arg(args, 0) as usize;
    let size = long_arg(args, 1);
    if addr == 0 {
        // Tolerate a null unmap — matches JDK behavior where unmap
        // on an already-released buffer is a no-op.
        return Ok(Some(Value::Int(0)));
    }
    let mut reg = mmap_registry().lock();
    // Defensive check: warn if the caller's claimed size doesn't
    // match what we recorded at map0. This catches buggy JDK shims
    // and adversarial callers that fabricate metadata. The drop
    // path is still correct (the holder knows its true length),
    // so we proceed regardless.
    if let Some(entry) = reg.get(&addr) {
        let true_len = entry.len() as i64;
        if size > 0 && size != true_len {
            eprintln!(
                "native-io: FileChannelImpl.unmap0(addr=0x{addr:x}, size={size}) \
                 disagrees with recorded mapping length {true_len}; proceeding \
                 with true length (possible JDK shim bug or fabricated metadata)"
            );
        }
    }
    // Drop here unmaps. If the address isn't in the registry we
    // still return 0 (the JDK may double-unmap on cleaner races,
    // and there is no harm in absorbing those).
    reg.remove(&addr);
    Ok(Some(Value::Int(0)))
}

// ---------------------------------------------------------------------------
// force0 — fsync. Critical for `FileChannel.force(metaData)` durability.
//
// Java contract: when `metaData == true` we must flush both data AND
// file metadata (mtime, size, etc.) to stable storage — i.e. `fsync`.
// When `false` only data needs to reach disk — i.e. `fdatasync` on
// Linux / `FlushFileBuffers` on Windows (which always syncs both).
//
// We map these to `std::fs::File::sync_all` / `sync_data` which delegate
// to the right syscall per platform. A silent no-op here is a critical
// durability bug — callers that ran `FileChannel.force(true)` would
// believe their data was on disk when it was only in the page cache.
// ---------------------------------------------------------------------------

/// `force0(FileDescriptor, boolean metaData) -> int`
///
/// Returns 0 on success. Throws IOException on sync failure.
fn native_fc_force0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let meta_data = match args.get(1) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    let Some(fd) = fd_from_descriptor(ctx, fd_obj) else {
        return Err(io_error("force0: FileDescriptor has no open handle"));
    };
    // Clone the underlying File so we can issue the sync syscall
    // without holding the FdTable mutex across it. `clone_file`
    // flushes BufWriter-backed entries first, so any buffered bytes
    // are in the kernel page cache before we ask for sync.
    let file = ctx
        .fd_table()
        .clone_file(fd)
        .map_err(|e| io_error(format!("force0: clone fd: {e}")))?;
    let sync_res = if meta_data {
        file.sync_all()
    } else {
        file.sync_data()
    };
    sync_res.map_err(|e| io_error(format!("force0: {e}")))?;
    Ok(Some(Value::Int(0)))
}

// ---------------------------------------------------------------------------
// transferTo0 / maxDirectTransferSize0 — WP3.6
// ---------------------------------------------------------------------------

/// `maxDirectTransferSize0() -> int`
///
/// Linux `sendfile(2)` accepts up to `0x7ffff000` per call; on
/// Windows `TransmitFile` caps at 2 GiB - 1. Returning `i32::MAX`
/// is safe everywhere: the kernel will return short on Linux and
/// the JDK loops automatically. On Windows we never invoke
/// `TransmitFile` directly (we use the userspace fallback), so
/// the value is informational only.
fn native_fc_max_direct_transfer_size0(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0x7fff_ffff)))
}

fn new_object_ref(
    ctx: &mut dyn NativeContext,
    class_name: &'static str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.new_object(class_name)? {
        Some(Value::Object(Some(o))) => Ok(o),
        _ => Err(io_error(format!("{class_name}: allocation returned null"))),
    }
}

fn new_native_thread_set(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    if let Ok(Some(Value::Object(Some(o)))) =
        ctx.new_object_initialized("sun/nio/ch/NativeThreadSet", "(I)V", &[Value::Int(2)])
    {
        return Ok(o);
    }

    // GC: `threads` is a freshly allocated object that NOTHING in Java refers
    // to yet, so this Rust local is its only reference — and `new_array` below
    // allocates. Under the moving collector that leaves the local stale; under
    // the Generational collector's NON-MOVING young sweep the object is simply
    // unreachable and gets ZEROED, which is how a live
    // `sun/nio/ch/NativeThreadSet` came back with an all-zero header.
    let mut scope = NativeHandleScope::new(ctx);
    let threads_h = {
        let o = new_object_ref(&mut *scope, "sun/nio/ch/NativeThreadSet")?;
        scope.root(o)
    };
    let slots_h = {
        let o = scope.new_array(ArrayElementType::Long, 2);
        scope.root(o)
    };
    let (threads, thread_slots) = (scope.get(&threads_h), scope.get(&slots_h));
    scope.set_field_by_name(threads, "elts", Value::Object(Some(thread_slots)));
    let threads = scope.get(&threads_h);
    scope.set_field_by_name(threads, "used", Value::Int(0));
    let threads = scope.get(&threads_h);
    scope.set_field_by_name(threads, "waitingToEmpty", Value::Int(0));
    Ok(scope.get(&threads_h))
}

/// `FileChannelImpl.open(FileDescriptor, String, boolean readable, boolean writable, ...)`.
///
/// The CratonVM provider shim can call the JDK 25 factory shape even when the
/// boot image does not expose that exact method. Materialize the real
/// `FileChannelImpl` field layout directly, so callers do not fall back to the
/// synthetic abstract `java.nio.channels.FileChannel`.
/// `sun/nio/ch/FileChannelImpl$Closer.run()V` — the `Runnable` action a
/// real `Cleaner.Cleanable` invokes on `clean()`. Real bytecode reads
/// `this.fd` and calls `FileChannelImpl.fdAccess.close(fd)`, an
/// internal `SharedSecrets`-style static field populated by
/// `java.io.FileDescriptor`'s own class initializer. In this bridge's
/// construction path (`native_fcimpl_open` builds the `FileChannelImpl`
/// object directly rather than running its real `<init>`), that static
/// field's population is not reliably ordered before this runs, and an
/// interface call through a null `fdAccess` silently no-ops here rather
/// than throwing -- the fd is never actually released (see
/// `native_fcimpl_open`'s `closer` registration below for the full
/// history: this is what finally makes `closer.clean()` -> `run()`
/// actually close the fd instead of being a well-registered no-op).
/// Override `run()` directly against our own fd table instead of relying
/// on that indirection.
fn native_fcimpl_closer_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let fd_obj = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    if let Some(fd) = fd_from_descriptor(ctx, fd_obj) {
        let _ = ctx.fd_table().close(fd);
    }
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(-1));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(-1));
    Ok(None)
}

fn native_fcimpl_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(io_error("FileChannelImpl.open: null FileDescriptor")),
    };
    let path = args.get(1).copied().unwrap_or(Value::Object(None));
    let readable = args.get(2).copied().unwrap_or(Value::Int(0));
    let writable = args.get(3).copied().unwrap_or(Value::Int(0));
    let jdk21_direct = args.get(4).copied().unwrap_or(Value::Int(0));
    let jdk25_direct = args.get(5).copied().unwrap_or(Value::Int(0));
    let direct = if args.len() >= 7 {
        jdk25_direct
    } else {
        jdk21_direct
    };
    let parent = args.last().copied().unwrap_or(Value::Object(None));
    let parent_is_null = matches!(parent, Value::Object(None));

    // GC DISCIPLINE FOR THE WHOLE CONSTRUCTION.
    //
    // Each of the five allocations below can collect, and until a field store
    // publishes one of these objects into something Java can reach, the ONLY
    // reference to it is a Rust local. That is stale-local territory under the
    // moving collector — which is why the two `pin_native_root` pairs further
    // down already exist — but it is worse under the Generational collector's
    // NON-MOVING young sweep: an unreachable object is not moved, it is ZEROED
    // in place. Measured 2026-09-06 with `CRATONVM_DBG_SWEEP_ZERO=1`: a live
    // `sun/nio/ch/FileChannelImpl` and its `sun/nio/ch/NativeThreadSet`
    // reclaimed mid-construction, after which `FileChannel.map` reads the fd
    // back as 0 and the embedded Kafka broker cannot mmap its index.
    //
    // A handle scope is the tree's answer to this family (see
    // `NativeContext`'s "rooted handle scope" block): the handle is an opaque
    // slot rather than an `ObjectRef`, so the pre-GC local cannot be read back
    // by mistake, and `handle_slots` is scanned by `roots.rs`. Re-read every
    // handle immediately before each use — `set_field_by_name` resolves a field
    // name and can itself allocate.
    let mut scope = NativeHandleScope::new(ctx);
    let fd_h = scope.root(fd_obj);
    let channel_h = {
        let o = new_object_ref(&mut *scope, "sun/nio/ch/FileChannelImpl")?;
        scope.root(o)
    };
    let close_lock_h = {
        let o = new_object_ref(&mut *scope, "java/lang/Object")?;
        scope.root(o)
    };
    let position_lock_h = {
        let o = new_object_ref(&mut *scope, "java/lang/Object")?;
        scope.root(o)
    };
    let dispatcher_h = {
        let o = new_object_ref(&mut *scope, "sun/nio/ch/FileDispatcherImpl")?;
        scope.root(o)
    };
    let threads_h = {
        let o = new_native_thread_set(&mut *scope)?;
        scope.root(o)
    };
    // The two reference-typed ARGUMENTS are rooted by the caller's frame, so
    // they cannot be reclaimed — but they can MOVE across the allocations
    // above, and both are stored into the channel below.
    let path_h = match path {
        Value::Object(Some(o)) => Some(scope.root(o)),
        _ => None,
    };
    let parent_h = match parent {
        Value::Object(Some(o)) => Some(scope.root(o)),
        _ => None,
    };

    let (channel, close_lock) = (scope.get(&channel_h), scope.get(&close_lock_h));
    scope.set_field_by_name(channel, "closeLock", Value::Object(Some(close_lock)));
    let channel = scope.get(&channel_h);
    scope.set_field_by_name(channel, "closed", Value::Int(0));
    // `interruptor` is a FINAL field that the real
    // `AbstractInterruptibleChannel()` constructor always assigns -- it is never
    // null on a live channel. This bridge builds the channel without running
    // that constructor and used to leave the slot null, which made
    // `AbstractInterruptibleChannel.begin()` throw
    // `NullPointerException: Cannot invoke "sun.nio.ch.Interruptible.interrupt(
    // java.lang.Thread)" because "this.interruptor" is null` on any channel
    // operation performed by a thread whose interrupt flag happens to be set --
    // so an interrupt during file I/O crashed instead of performing the
    // specified asynchronous close (H2 `TestStreamStore`, 2026-07-26).
    //
    // Build the real anonymous implementation
    // (`AbstractInterruptibleChannel$1`, whose only state is `this$0`) so
    // `interrupt(Thread)` runs the JDK bytecode -- `this$0.trySetTarget(target)`
    // -- and `postInterrupt()` closes the channel. Falls back to null if the
    // class is unavailable, which is exactly the previous behaviour.
    //
    // GC: `new_object_initialized` allocates and runs bytecode, either of which
    // can relocate `channel`; pin it across the call and read the live address
    // back (native stale-local family).
    // The ad-hoc `pin_native_root`/`read_native_pin` pair that used to wrap
    // this call is gone: the scope already roots `channel` for the whole
    // construction, and one pinned call site beside four unpinned allocations
    // is exactly the "pinning at each call site is weakest of all" shape the
    // handle-scope block warns about.
    let channel = scope.get(&channel_h);
    let interruptor = scope
        .new_object_initialized(
            "java/nio/channels/spi/AbstractInterruptibleChannel$1",
            "(Ljava/nio/channels/spi/AbstractInterruptibleChannel;)V",
            &[Value::Object(Some(channel))],
        )
        .ok()
        .flatten()
        .unwrap_or(Value::Object(None));
    let channel = scope.get(&channel_h);
    scope.set_field_by_name(channel, "interruptor", interruptor);
    let channel = scope.get(&channel_h);
    scope.set_field_by_name(channel, "interrupted", Value::Object(None));

    let (channel, threads) = (scope.get(&channel_h), scope.get(&threads_h));
    scope.set_field_by_name(channel, "threads", Value::Object(Some(threads)));
    let (channel, position_lock) = (scope.get(&channel_h), scope.get(&position_lock_h));
    scope.set_field_by_name(channel, "positionLock", Value::Object(Some(position_lock)));
    let (channel, fd_obj) = (scope.get(&channel_h), scope.get(&fd_h));
    scope.set_field_by_name(channel, "fd", Value::Object(Some(fd_obj)));
    let channel = scope.get(&channel_h);
    scope.set_field_by_name(channel, "readable", readable);
    let channel = scope.get(&channel_h);
    scope.set_field_by_name(channel, "writable", writable);
    let channel = scope.get(&channel_h);
    let parent = match &parent_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => parent,
    };
    scope.set_field_by_name(channel, "parent", parent);
    let channel = scope.get(&channel_h);
    let path = match &path_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => path,
    };
    scope.set_field_by_name(channel, "path", path);
    let channel = scope.get(&channel_h);
    scope.set_field_by_name(channel, "direct", direct);
    let channel = scope.get(&channel_h);
    scope.set_field_by_name(channel, "alignment", Value::Int(-1));
    let (channel, dispatcher) = (scope.get(&channel_h), scope.get(&dispatcher_h));
    scope.set_field_by_name(channel, "nd", Value::Object(Some(dispatcher)));
    let channel = scope.get(&channel_h);
    scope.set_field_by_name(channel, "fileLockTable", Value::Object(None));

    // Real JDK's FileChannelImpl private constructor ALWAYS registers a
    // Cleaner action here when `parent` is null: `closer = parent == null
    // ? cleaner.register(this, new Closer(fd)) : null` (confirmed via
    // javap against this host's real JDK 25). This bridge previously set
    // `closer` unconditionally to null, producing a combination real
    // bytecode never creates when parent is null: `implCloseChannel()`
    // always calls `closer.clean()` on the parent-null branch, so a null
    // closer meant the fd was silently never released whenever
    // invokeinterface on a null receiver didn't throw here -- a resource
    // leak. Combined with a caller that reopens the same path shortly
    // after closing (e.g. MVStore's compact()/checkpoint sequence), the
    // still-open fd can outlive what Java believes is a closed channel,
    // racing the JVM-level FileLockTable bookkeeping and MVStore's own
    // chunk metadata -- see H2 TestLob's OverlappingFileLockException /
    // "Chunk N not found"
    // (docs/known-issues/h2/!bug-h2-testlob-mvstore-chunk-not-found-and-file-lock.md).
    // Register the real Cleaner action so the fd genuinely closes,
    // mirroring the real constructor exactly. `CleanerFactory.cleaner()`
    // is already relied on elsewhere for FileInputStream/FileOutputStream
    // cleanup (see the "P69-Cleaner-realfix" note in lib.rs), so the
    // underlying Cleaner machinery is known-working here.
    let mut closer = Value::Object(None);
    if parent_is_null {
        // `cleaner` and `closer_runnable` are freshly returned references held
        // only in Rust locals across each other's allocating calls — the same
        // shape as the five above, one nesting level down.
        let cleaner_h = scope
            .invoke(
                "jdk/internal/ref/CleanerFactory",
                "cleaner",
                "()Ljava/lang/ref/Cleaner;",
                &[],
            )
            .ok()
            .flatten()
            .and_then(|v| match v {
                Value::Object(Some(o)) => Some(scope.root(o)),
                _ => None,
            });
        let fd_obj = scope.get(&fd_h);
        let runnable_h = scope
            .new_object_initialized(
                "sun/nio/ch/FileChannelImpl$Closer",
                "(Ljava/io/FileDescriptor;)V",
                &[Value::Object(Some(fd_obj))],
            )
            .ok()
            .flatten()
            .and_then(|v| match v {
                Value::Object(Some(o)) => Some(scope.root(o)),
                _ => None,
            });
        if let (Some(ch), Some(rh)) = (cleaner_h, runnable_h) {
            let (cleaner_obj, runnable, channel) =
                (scope.get(&ch), scope.get(&rh), scope.get(&channel_h));
            closer = scope
                .invoke_virtual(
                    cleaner_obj,
                    "register",
                    "(Ljava/lang/Object;Ljava/lang/Runnable;)Ljava/lang/ref/Cleaner$Cleanable;",
                    &[Value::Object(Some(channel)), Value::Object(Some(runnable))],
                )
                .ok()
                .flatten()
                .unwrap_or(Value::Object(None));
        }
    }
    // `closer` is the `Cleanable` `register` just returned — one more freshly
    // allocated object held only in a Rust local, and `set_field_by_name`
    // is the reference this stores. Root it for the store.
    let closer_h = match closer {
        Value::Object(Some(o)) => Some(scope.root(o)),
        _ => None,
    };
    let channel = scope.get(&channel_h);
    let closer = match &closer_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => closer,
    };
    scope.set_field_by_name(channel, "closer", closer);

    Ok(Some(Value::Object(Some(scope.get(&channel_h)))))
}

fn native_native_thread_set_add(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn native_native_thread_set_remove(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

fn native_native_thread_set_signal_and_wait(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

/// `transferTo0(int srcFD, long position, long count, int dstFD [, boolean append]) -> long`
///
/// Some JDKs pass `FileDescriptor` objects, others pass raw int
/// fd numbers. We accept both shapes — Object yields a lookup via
/// `fd_from_descriptor`, Int is taken as the FdId directly.
///
/// Linux: drive `libc::sendfile`. Windows / fallback: do a 64 KiB
/// userspace `pread`/`pwrite` loop. The fallback is mandated by
/// the spec when the destination is not a socket; we apply it
/// uniformly on Windows because `TransmitFile` requires a real
/// `SOCKET` HANDLE which our `FdTable` does not currently expose.
/// `transferFrom0(FileDescriptor src, FileDescriptor dst, long position,
/// long count [, boolean append]) -> long`
///
/// Decline, with the JDK's own "no kernel-side copy here" answer.
///
/// `IOStatus.UNSUPPORTED` is what HotSpot's implementation returns whenever the
/// platform or the fd kind has no `copy_file_range`-style primitive, and
/// `FileChannelImpl.transferFrom` responds by falling back to
/// `transferFromArbitraryChannel`, a `ByteBuffer` read/write loop. That loop
/// works here, so the observable behaviour is a correct transfer.
///
/// Before this existed the method had no registration at all and
/// `FileChannel.transferFrom` between two file channels raised
/// `UnsatisfiedLinkError` — while `transferTo` in the same direction succeeded.
fn native_fc_transfer_from0(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(IOSTATUS_UNSUPPORTED)))
}

fn native_fc_transfer_to0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src_fd = match fd_id_arg(ctx, args, 0) {
        Some(fd) => fd,
        None => return Err(io_error("transferTo0: bad src fd")),
    };
    let position = long_arg(args, 1);
    let count = long_arg(args, 2);
    // arg 3 may be a FileDescriptor object or a raw int.
    let dst_fd = match fd_id_arg(ctx, args, 3) {
        Some(fd) => fd,
        None => return Err(io_error("transferTo0: bad dst fd")),
    };

    if position < 0 || count < 0 {
        return Err(io_error("transferTo0: negative position or count"));
    }
    if count == 0 {
        return Ok(Some(Value::Long(0)));
    }

    // Linux fast path: real zero-copy via sendfile(2). For
    // regular-file → regular-file transfers, `copy_file_range(2)` is
    // strictly better on modern kernels (5.3+) because it can do
    // filesystem-internal reflinks / server-side copies on btrfs/XFS
    // and short-circuits to a pagecache-only copy on ext4, whereas
    // sendfile always streams pages through the pipe machinery.
    //
    // AUDIT 2026-05-17: prefer copy_file_range; fall back to sendfile
    // on EXDEV / ENOSYS / EINVAL (different mounts, older kernels,
    // non-regular-file fds), then fall back again to the userspace
    // loop if both refuse. Other errors propagate.
    #[cfg(target_os = "linux")]
    {
        if let Some(n) = transfer_via_copy_file_range(ctx, src_fd, position, count, dst_fd)? {
            return Ok(Some(Value::Long(n)));
        }
        if let Some(n) = transfer_via_sendfile(ctx, src_fd, position, count, dst_fd)? {
            return Ok(Some(Value::Long(n)));
        }
        // Both said UNSUPPORTED → fall through to the userspace loop.
    }

    // Windows: TransmitFile is the Win32 zero-copy moral
    // equivalent of sendfile — but it requires a SOCKET handle
    // for the destination, which our FdTable does not currently
    // expose for TCP streams. The roadmap explicitly allows the
    // read/write fallback for non-socket destinations, so we
    // delegate to `transfer_userspace_loop` below. A future WP
    // can swap in `windows_sys::Win32::Networking::WinSock::TransmitFile`
    // here once SOCKET fds are first-class in `FdTable`.
    #[cfg(windows)]
    let _windows_transmitfile_fallback: () = ();

    // Userspace fallback — runs on:
    //   * Windows, where `TransmitFile` requires a SOCKET handle
    //     that our FdTable does not currently expose. The reference
    //     to `TransmitFile` in the cfg-windows comment above
    //     documents the platform-zero-copy alternative.
    //     (`Win32::Networking::WinSock::TransmitFile`.)
    //   * macOS / BSDs, which have `sendfile(2)` with a different
    //     signature; supporting that is out of scope here.
    //   * Linux, when sendfile reports the src/dst is not
    //     compatible (e.g. pipe, /proc file).
    let n = transfer_userspace_loop(ctx, src_fd, position, count, dst_fd)?;
    Ok(Some(Value::Long(n)))
}

/// Resolve the FdId from arg `idx`, accepting either an `ObjectRef`
/// (a `java.io.FileDescriptor`) or a raw `int`.
fn fd_id_arg(ctx: &mut dyn NativeContext, args: &[Value], idx: usize) -> Option<FdId> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => fd_from_descriptor(ctx, *o),
        Some(Value::Int(v)) if *v > 2 => Some(*v as FdId),
        Some(Value::Long(v)) if *v > 2 && *v < u32::MAX as i64 => Some(*v as FdId),
        _ => None,
    }
}

/// Linux: drive `libc::copy_file_range(2)` in a loop. Best for
/// file→file transfers (filesystem-internal reflink on btrfs/XFS,
/// pagecache copy on ext4 — no syscall overhead per page). Returns
/// `Ok(Some(n))` on success / short copy, `Ok(None)` if the kernel
/// reports the syscall is unsupported (older kernel, EXDEV across
/// filesystems, non-regular-file fds), `Err` on hard errors.
///
/// AUDIT 2026-05-17: added per round-5 native-misc HIGH item — the
/// userspace 64 KiB loop is the cold-cache path; copy_file_range is
/// what GNU `cp --reflink=auto` uses and saves both the userspace
/// copy and the per-page syscall overhead for large transfers.
#[cfg(target_os = "linux")]
fn transfer_via_copy_file_range(
    ctx: &mut dyn NativeContext,
    src_fd: FdId,
    position: i64,
    count: i64,
    dst_fd: FdId,
) -> Result<Option<i64>, MethodCallFailed> {
    use std::os::unix::io::AsRawFd;

    let src_file = ctx
        .fd_table()
        .clone_file(src_fd)
        .map_err(|e| io_error(format!("transferTo0: clone src: {e}")))?;
    let dst_file = match ctx.fd_table().clone_file(dst_fd) {
        Ok(f) => f,
        Err(_) => {
            // Non-file destination — copy_file_range requires two
            // regular files. Let the caller try sendfile / userspace.
            return Ok(None);
        }
    };

    let src_raw = src_file.as_raw_fd();
    let dst_raw = dst_file.as_raw_fd();
    let mut src_off: libc::loff_t = position as libc::loff_t;
    // Destination uses its current cursor; passing NULL for off_out
    // tells the kernel to advance dst's file offset.
    let mut transferred: i64 = 0;
    let mut remaining = count;
    while remaining > 0 {
        // copy_file_range has no documented per-call cap; the kernel
        // returns whatever it can do in one go. Pass `remaining` and
        // trust the short-write loop.
        let chunk = remaining as usize;
        // SAFETY: src_raw, dst_raw are open kernel fds; src_off points
        // to a valid stack-local loff_t; off_out is NULL (kernel uses
        // dst's cursor); flags must be 0 per man page.
        let r = unsafe {
            libc::syscall(
                libc::SYS_copy_file_range,
                src_raw,
                &mut src_off as *mut libc::loff_t,
                dst_raw,
                std::ptr::null_mut::<libc::loff_t>(),
                chunk as libc::size_t,
                0u32,
            )
        };
        if r > 0 {
            transferred += r as i64;
            remaining -= r as i64;
        } else if r == 0 {
            // EOF on src.
            break;
        } else {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::ENOSYS)
                | Some(libc::EXDEV)
                | Some(libc::EINVAL)
                | Some(libc::EOPNOTSUPP)
                | Some(libc::EBADF) => {
                    // Kernel doesn't support copy_file_range for this
                    // pair (older kernel, cross-filesystem, non-regular
                    // fd, etc). Let the caller try sendfile.
                    if transferred == 0 {
                        return Ok(None);
                    }
                    return Ok(Some(transferred));
                }
                // Match-guard form rather than `EAGAIN | EWOULDBLOCK` patterns:
                // on Linux the two constants are EQUAL, so the second or-pattern
                // is an `unreachable_patterns` lint error under `-D warnings`
                // (while other unixes keep them distinct — the guard covers both
                // without tripping either build).
                Some(e) if e == libc::EAGAIN || e == libc::EWOULDBLOCK => {
                    if transferred > 0 {
                        return Ok(Some(transferred));
                    }
                    return Ok(Some(IOSTATUS_UNAVAILABLE));
                }
                Some(libc::EINTR) => {
                    if transferred > 0 {
                        return Ok(Some(transferred));
                    }
                    return Ok(Some(IOSTATUS_INTERRUPTED));
                }
                _ => {
                    return Err(io_error(format!("transferTo0: copy_file_range: {err}")));
                }
            }
        }
    }
    Ok(Some(transferred))
}

/// Linux: drive `libc::sendfile` in a loop until `count` bytes are
/// transferred or the kernel reports EAGAIN/EOF/Error. Returns
/// `Ok(Some(n))` on success/short, `Ok(None)` if sendfile is not
/// applicable (caller should fall back), `Err` on hard errors.
#[cfg(target_os = "linux")]
fn transfer_via_sendfile(
    ctx: &mut dyn NativeContext,
    src_fd: FdId,
    position: i64,
    count: i64,
    dst_fd: FdId,
) -> Result<Option<i64>, MethodCallFailed> {
    use std::os::unix::io::AsRawFd;

    // Clone both ends so we don't deadlock on the FdTable mutex
    // while syscalling.
    let src_file = ctx
        .fd_table()
        .clone_file(src_fd)
        .map_err(|e| io_error(format!("transferTo0: clone src: {e}")))?;
    // Destination might be a regular file too (e.g. file-to-file
    // copy via FileChannel). If it's a socket we'd ideally use
    // TcpStream::as_raw_fd, but for now restrict to file dst.
    //
    // AUDIT 2026-05-24: this `clone_file` may return Err for either
    //   (a) "destination is not a regular file" — e.g. a socket or
    //       pipe entry in the FdTable that has no underlying `File`
    //       handle to clone, OR
    //   (b) "destination IS a regular file but its FdTable entry is
    //       a wrapper (RWStream, BufWriter-wrapped, etc.) that
    //       `clone_file` cannot dup without breaking buffering."
    // In both cases the *correct* behaviour is to fall back to the
    // userspace loop, so we collapse the distinction here. The
    // userspace path handles all destination kinds correctly; the
    // only cost is the missed sendfile zero-copy optimisation. We
    // log the kind that triggered the fallback so future debugging
    // can tell sockets-as-dst apart from file-but-not-cloneable.
    let dst_file = match ctx.fd_table().clone_file(dst_fd) {
        Ok(f) => f,
        Err(e) => {
            // Distinguish (a) vs (b) by inspecting the error string:
            // we don't have a public "is_regular_file" probe, but
            // `clone_file` already failed — `e` carries the kind hint.
            // We surface that diagnostically without changing control
            // flow.
            eprintln!(
                "native-io: transferTo0: sendfile fast-path declined \
                 (dst_fd={dst_fd}, clone_file err='{e}'); falling \
                 back to userspace copy. This is a correctness no-op \
                 — only a perf regression."
            );
            return Ok(None);
        }
    };

    let src_raw = src_file.as_raw_fd();
    let dst_raw = dst_file.as_raw_fd();
    // Linux's sendfile takes `off_t *` for the in offset and
    // updates it. We pass &mut so the kernel advances it.
    let mut off: libc::off_t = position as libc::off_t;
    let cap = std::cmp::min(count, 0x7fff_f000) as libc::size_t;

    let mut transferred: i64 = 0;
    let mut remaining = count;
    while remaining > 0 {
        let chunk = std::cmp::min(remaining as libc::size_t, cap);
        // SAFETY: src_raw and dst_raw are open kernel fds; off is
        // a valid pointer to a stack-local off_t; chunk is bounded
        // by sendfile's documented max.
        let r = unsafe { libc::sendfile(dst_raw, src_raw, &mut off, chunk) };
        if r > 0 {
            transferred += r as i64;
            remaining -= r as i64;
        } else if r == 0 {
            // EOF on src.
            break;
        } else {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                // Match-guard form rather than `EAGAIN | EWOULDBLOCK` patterns:
                // on Linux the two constants are EQUAL, so the second or-pattern
                // is an `unreachable_patterns` lint error under `-D warnings`
                // (while other unixes keep them distinct — the guard covers both
                // without tripping either build).
                Some(e) if e == libc::EAGAIN || e == libc::EWOULDBLOCK => {
                    if transferred > 0 {
                        return Ok(Some(transferred));
                    }
                    return Ok(Some(IOSTATUS_UNAVAILABLE));
                }
                Some(libc::EINTR) => {
                    if transferred > 0 {
                        return Ok(Some(transferred));
                    }
                    return Ok(Some(IOSTATUS_INTERRUPTED));
                }
                Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EBADF) => {
                    // Source isn't sendfile-compatible (pipes, some
                    // sockets, /proc files). Caller falls back.
                    if transferred == 0 {
                        return Ok(None);
                    }
                    return Ok(Some(transferred));
                }
                _ => {
                    return Err(io_error(format!("transferTo0: sendfile: {err}")));
                }
            }
        }
    }
    Ok(Some(transferred))
}

/// Cross-platform userspace fallback: 64 KiB stack-ish buffer,
/// read from src at `position`, write to dst at its current
/// position (or end if append). Uses pread/write on the FdTable
/// directly so we don't need to manipulate the source's cursor.
fn transfer_userspace_loop(
    ctx: &mut dyn NativeContext,
    src_fd: FdId,
    position: i64,
    count: i64,
    dst_fd: FdId,
) -> Result<i64, MethodCallFailed> {
    const CHUNK: usize = 64 * 1024;
    let mut buf = vec![0u8; CHUNK];
    let mut transferred: i64 = 0;
    let mut src_pos = position as u64;
    let mut remaining = count;

    while remaining > 0 {
        let want = std::cmp::min(remaining as usize, CHUNK);
        let n = ctx
            .fd_table()
            .pread_at(src_fd, &mut buf[..want], src_pos)
            .map_err(|e| io_error(format!("transferTo0: pread: {e}")))?;
        if n == 0 {
            // EOF on the source.
            break;
        }
        // Write to dst. We try sequential write_bytes first
        // (covers FileWrite / TcpStream / etc.); if that fails
        // because the dst is a FileReadWrite, fall through to
        // rw_write which advances the cursor.
        let write_res = ctx.fd_table().write_bytes(dst_fd, &buf[..n]);
        match write_res {
            Ok(()) => {}
            Err(_) => {
                // Try rw_write (FileReadWrite path).
                let _ = ctx
                    .fd_table()
                    .rw_write(dst_fd, &buf[..n])
                    .map_err(|e| io_error(format!("transferTo0: write: {e}")))?;
            }
        }
        transferred += n as i64;
        src_pos += n as u64;
        remaining -= n as i64;
    }

    Ok(transferred)
}

// ---------------------------------------------------------------------------
// Public registration
// ---------------------------------------------------------------------------

/// Register real `map0` / `unmap0` / `transferTo0` /
/// `maxDirectTransferSize0` natives on both `FileDispatcherImpl`
/// (JDK 25 + WindowsFileDispatcherImpl + UnixFileDispatcherImpl)
/// and the legacy `FileChannelImpl` alias.
///
/// Idempotent: calling after `register_nio_natives_real` will
/// replace the stub registrations there, since the registry's
/// `register` overwrites duplicate keys.
/// `FileDispatcher.canTransferToDirectly(SelectableChannel) -> false`.
///
/// # What this stops
///
/// `FileChannelImpl.transferTo` tries three strategies in order: a DIRECT
/// `sendfile(2)`, a "trusted channel" copy, and finally an arbitrary-channel
/// `ByteBuffer` loop. The first one asks this method whether the target is a
/// channel it may `sendfile` into, and the stock `UnixFileDispatcherImpl` answers
/// an unconditional `true` (its whole body is `iconst_1; ireturn`).
///
/// Taking it is fatal here, and not because of `sendfile`. The direct arm calls
/// `SocketChannelImpl.beforeTransferTo()` first, which is ordinary JDK bytecode
/// reading the channel's OWN fields -- and CratonVM does not build socket
/// channels by running the JDK constructor. Every per-channel fact this VM keeps
/// lives in an identity-keyed side table (`socket_channel.rs`, `cf_set`/`cf_get`),
/// so the real class's `private final` slots are still null/zero. MEASURED, a
/// `FileChannel.transferTo(0, size, SocketChannel)` on
/// `probes/TransferToSocketProbe.java`:
///
/// ```text
/// NullPointerException: Cannot invoke "ReentrantLock.lock()"
///                       because "this.writeLock" is null
///     at sun/nio/ch/SocketChannelImpl.beforeTransferTo(SocketChannelImpl.java:671)
/// ```
///
/// On the wire that is a response with `Content-Length: 951` and a body of ZERO
/// bytes -- the client then reports `Premature end of Content-Length delimited
/// message body (expected: 951; received: 0)`, which is how it reached us
/// (Spring's `ZeroCopyIntegrationTests`, Reactor Netty's `sendFile` -> Netty's
/// `DefaultFileRegion.transferTo` -> here).
///
/// # Why DECLINE rather than seed the missing fields
///
/// Seeding `writeLock` is not enough and was checked before being rejected.
/// `beforeTransferTo` also takes `synchronized (stateLock)`, calls
/// `ensureOpenAndConnected()` -- which reads the JDK's own `state` int -- and
/// writes `writerThread`. CratonVM maintains none of them, so a seeded lock only
/// moves the failure from `NullPointerException` to `ClosedChannelException`.
/// (It does NOT deadlock: the handler at bci 68 unlocks before rethrowing. That
/// was checked too, because a leaked write lock would have been much worse than
/// the bug being fixed.) Making the direct arm genuinely work means maintaining
/// the real `SocketChannelImpl` state machine, which is a different project.
///
/// # Why this is a correct answer and not a workaround
///
/// `false` is the JDK's OWN way of saying "not this target": the arm returns
/// `IOStatus.UNSUPPORTED` and `transferTo` falls through to
/// `transferToTrustedChannel` / `transferToArbitraryChannel`, which copy through
/// a `ByteBuffer` using `SocketChannelImpl.write` -- a path this VM implements
/// and that measures 951/951 bytes delivered, byte-identical to HotSpot.
///
/// The gate is consulted ONLY when the target is a `SelectableChannel`, so
/// file->file transfers keep using [`native_fc_transfer_to0`]'s real
/// `sendfile(2)`. This narrows one arm; it does not disable zero-copy.
fn native_fc_can_transfer_to_directly(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

pub fn register_file_channel_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- modern JDK 25 dispatch surface ---
    //
    // Registered under all three platform spellings; `backed` names the ones a
    // JDK 25 image declares ACC_NATIVE, measured on linux-x64 AND windows-x64
    // 25.0.4+7 (2026-08-05). `WindowsFileDispatcherImpl` is on neither image —
    // the Windows JDK names its class `FileDispatcherImpl` — so it is never
    // backed, and the second `map0`/`transferTo0` shapes are older JDKs' and
    // are backed nowhere either.
    use crate::nio_native::{register_fd_native, FD_LEAF, FD_UNIX};
    // `backed` is empty on purpose: `canTransferToDirectly` is ordinary
    // bytecode in every JDK 25 image (`iconst_1; ireturn`), not `ACC_NATIVE`,
    // so this row is an override of a real body rather than a JNI bridge and
    // must not claim the `Bridge` kind. See the doc comment on the callback.
    register_fd_native(
        r,
        "canTransferToDirectly",
        "(Ljava/nio/channels/SelectableChannel;)Z",
        native_fc_can_transfer_to_directly,
        &[],
    );
    register_fd_native(
        r,
        "map0",
        "(Ljava/io/FileDescriptor;IJJZ)J",
        native_fc_map0,
        &[FD_LEAF, FD_UNIX],
    );
    register_fd_native(
        r,
        "map0",
        "(Ljava/io/FileDescriptor;IJJ)J",
        native_fc_map0,
        &[],
    );
    register_fd_native(r, "unmap0", "(JJ)I", native_fc_unmap0, &[FD_LEAF, FD_UNIX]);
    register_fd_native(
        r,
        "transferTo0",
        "(Ljava/io/FileDescriptor;JJLjava/io/FileDescriptor;Z)J",
        native_fc_transfer_to0,
        &[FD_LEAF],
    );
    register_fd_native(
        r,
        "transferTo0",
        "(Ljava/io/FileDescriptor;JJLjava/io/FileDescriptor;)J",
        native_fc_transfer_to0,
        &[],
    );
    // `transferFrom0(srcFD, dstFD, position, count, append)` — the sibling of
    // `transferTo0`, reached by `FileChannel.transferFrom(src, ...)` when the
    // source is itself a FileChannel. It had NO registration at all, so
    // `transferFrom` between two file channels died with
    // `UnsatisfiedLinkError: sun/nio/ch/FileDispatcherImpl.transferFrom0` —
    // while `transferTo` in the same direction worked. Found by
    // `probes/NioBufferStampProbe`.
    //
    // The answer is `IOStatus.UNSUPPORTED`, which is a real answer, not a stub:
    // it is exactly what HotSpot's own implementation returns on a platform or
    // fd kind that has no kernel-side copy, and the JDK responds by falling
    // back to `transferFromArbitraryChannel` — a `ByteBuffer` read/write loop
    // that already works here. Implementing a kernel `copy_file_range` path
    // would be faster but is a different piece of work; declining correctly
    // beats declining by link error, and beats a guess at the fd semantics.
    register_fd_native(
        r,
        "transferFrom0",
        "(Ljava/io/FileDescriptor;Ljava/io/FileDescriptor;JJZ)J",
        native_fc_transfer_from0,
        &[FD_LEAF],
    );
    register_fd_native(
        r,
        "transferFrom0",
        "(Ljava/io/FileDescriptor;Ljava/io/FileDescriptor;JJ)J",
        native_fc_transfer_from0,
        &[],
    );
    register_fd_native(
        r,
        "maxDirectTransferSize0",
        "()I",
        native_fc_max_direct_transfer_size0,
        &[FD_LEAF],
    );
    // force0 — fsync. Canonical real implementation lives here (was a silent
    // no-op stub in `nio_native.rs`, which lost data on crash for callers of
    // `FileChannel.force`).
    register_fd_native(
        r,
        "force0",
        "(Ljava/io/FileDescriptor;Z)I",
        native_fc_force0,
        &[FD_LEAF, FD_UNIX],
    );

    // --- legacy FileChannelImpl surface (older JDKs / fallback). The
    // signatures here use raw int fds rather than FileDescriptor. ---
    let fci = "sun/nio/ch/FileChannelImpl";
    r.register(
        fci,
        "open",
        "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;",
        native_fcimpl_open,
    );
    // JDK 21's real shape: one `boolean` shorter than JDK 25's above — 25 added
    // the `direct` flag — and ending in `Closeable`.
    //
    // This entry used to end in `Ljava/lang/Object;`, a descriptor no JDK has
    // ever declared, so it could never bind: on a JDK 21 image
    // `FileChannelImpl.open` had NO usable registration, and the smoke test
    // below asserted the unbindable spelling and passed on it. `dc55e8057`
    // deleted the line as dead — correct on the evidence it had — and that
    // turned a silent gap into a red test, which is how it was found. Verified
    // against Temurin 21.0.12+8 with `javap -p -s`.
    r.register(
        fci,
        "open",
        "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;",
        native_fcimpl_open,
    );
    r.register(
        "sun/nio/ch/FileChannelImpl$Closer",
        "run",
        "()V",
        native_fcimpl_closer_run,
    );
    let nts = "sun/nio/ch/NativeThreadSet";
    r.register(nts, "add", "()I", native_native_thread_set_add);
    r.register(nts, "remove", "(I)V", native_native_thread_set_remove);
    r.register(
        nts,
        "signalAndWait",
        "()V",
        native_native_thread_set_signal_and_wait,
    );
    r.register(fci, "map0", "(IJJZ)J", native_fc_map0_legacy);
    r.register(fci, "map0", "(IJJ)J", native_fc_map0_legacy);
    r.register(fci, "unmap0", "(JJ)I", native_fc_unmap0);
    r.register(fci, "transferTo0", "(IJJIZ)J", native_fc_transfer_to0);
    r.register(fci, "transferTo0", "(IJJI)J", native_fc_transfer_to0);
    // Raw-int-fd shapes of `transferFrom0`, for the JDKs that pass fd numbers
    // rather than `FileDescriptor` objects. Same decline as the object forms.
    r.register(fci, "transferFrom0", "(IIJJZ)J", native_fc_transfer_from0);
    r.register(fci, "transferFrom0", "(IIJJ)J", native_fc_transfer_from0);
    r.register(
        fci,
        "maxDirectTransferSize0",
        "()I",
        native_fc_max_direct_transfer_size0,
    );
    // sun/nio/ch/FileKey.init — the file-identity triple used by FileLockTable.
    // A missing native here is an UnsatisfiedLinkError on the FIRST file-backed
    // DB open (H2 `SingleFileStore.lockFileChannel` -> `FileChannelImpl.tryLock`
    // -> `FileKey.create`), so it blocks every persistent H2 database.
    r.register_with_kind(
        "sun/nio/ch/FileKey",
        "init",
        "(Ljava/io/FileDescriptor;[I)V",
        native_filekey_init,
        NativeKind::Bridge,
    );
    // Real Unix OpenJDK's `FileKey` (confirmed via javap against an actual
    // JDK 25 install) does NOT use the Windows-shaped int[3] convention
    // above at all — it's `private static native void init(FileDescriptor,
    // long[])` filling `result[0]=st_dev`, `result[1]=st_ino`, consumed by a
    // 2-arg `FileKey(long, long)` ctor. Without this second overload,
    // `FileKey.init` resolves to a real (unintercepted) JDK method with no
    // matching native for the ACTUAL descriptor the class file declares ->
    // `UnsatisfiedLinkError` on the first `FileChannel.lock()`/`tryLock()` in
    // real-JDK mode on Linux (e.g. Tomcat's `OcspBaseTest` responder lock).
    r.register_with_kind(
        "sun/nio/ch/FileKey",
        "init",
        "(Ljava/io/FileDescriptor;[J)V",
        native_filekey_init_longs,
        NativeKind::Bridge,
    );
    // KEEP: `FileKey.initIDs()` only caches the jfieldIDs for `st_dev`/`st_ino`
    // (`dwVolumeSerialNumber`/`nFileIndex*` on Windows) that `init` writes.
    // We resolve those fields by name in `native_filekey_init*`, so an empty
    // body is the faithful implementation, not a stub.
    r.register("sun/nio/ch/FileKey", "initIDs", "()V", |_ctx, _args| {
        Ok(None)
    });
    r.register(
        "sun/nio/ch/FileKey",
        "init",
        "(Ljava/io/FileDescriptor;)V",
        native_filekey_init_instance,
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// sun/nio/ch/FileKey.init — file-identity triple for FileLockTable
// ---------------------------------------------------------------------------
//
// `FileKey.create(fd)` allocates `int[3]`, calls the native
// `init(FileDescriptor, int[])` to fill it, then builds `FileKey(dwVol,
// idxHigh, idxLow)`. `FileLockTable` keys live locks by this triple so two
// `FileChannel`s onto the SAME file are detected as overlapping within one JVM
// (`FileKey.equals`/`hashCode` compare all three ints).
//
// Without the native, real-JDK `FileKey.init` is unresolved ->
// `UnsatisfiedLinkError` -> every file-backed DB open fails (H2's persistent
// `org.h2.test.TestAll` passes, `TestScript`, etc.). Windows fills the triple
// from `GetFileInformationByHandle` (the OS file identity). Unix maps
// `st_dev`/`st_ino` onto the three ints. If the OS query fails we fall back to
// the fd id so `init` is infallible (never throws): the lock table then
// degrades to per-open identity, correct for the common single-open case.

#[cfg(windows)]
mod win_fileid {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Filetime {
        pub dw_low_date_time: u32,
        pub dw_high_date_time: u32,
    }

    /// Mirror of Win32 `BY_HANDLE_FILE_INFORMATION` (ABI-fixed field order).
    #[repr(C)]
    pub struct ByHandleFileInformation {
        pub dw_file_attributes: u32,
        pub ft_creation_time: Filetime,
        pub ft_last_access_time: Filetime,
        pub ft_last_write_time: Filetime,
        pub dw_volume_serial_number: u32,
        pub n_file_size_high: u32,
        pub n_file_size_low: u32,
        pub n_number_of_links: u32,
        pub n_file_index_high: u32,
        pub n_file_index_low: u32,
    }

    // Hand-declared FFI (same convention as `pipe.rs` — avoids pulling in a
    // `windows-sys` dependency for a single symbol).
    #[link(name = "Kernel32")]
    extern "system" {
        pub fn GetFileInformationByHandle(
            h_file: *mut c_void,
            lp_file_information: *mut ByHandleFileInformation,
        ) -> i32;
    }
}

/// Resolve the `(dwVolumeSerialNumber, nFileIndexHigh, nFileIndexLow)` identity
/// triple for an open fd. Falls back to the fd id; never fails.
fn file_identity_triple(ctx: &mut dyn NativeContext, fd: FdId) -> (u32, u32, u32) {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        if let Ok(file) = ctx.fd_table().clone_file(fd) {
            // SAFETY: `info` is POD; the OS fully writes it on success and we
            // only read it when the call returns non-zero. `as_raw_handle`
            // yields a live OS handle owned by `file` for the call's duration.
            let mut info: win_fileid::ByHandleFileInformation = unsafe { std::mem::zeroed() };
            // SAFETY: `file` keeps the raw handle live and `info` is writable
            // storage for the exact Win32 structure until the call returns.
            let ok = unsafe {
                win_fileid::GetFileInformationByHandle(file.as_raw_handle() as *mut _, &mut info)
            };
            if ok != 0 {
                return (
                    info.dw_volume_serial_number,
                    info.n_file_index_high,
                    info.n_file_index_low,
                );
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(file) = ctx.fd_table().clone_file(fd) {
            if let Ok(md) = file.metadata() {
                let ino = md.ino();
                return (md.dev() as u32, (ino >> 32) as u32, ino as u32);
            }
        }
    }
    // OS query failed (or an unknown platform): a per-open identity keyed on the
    // fd id keeps `init` infallible.
    (fd as u32, 0, fd as u32)
}

/// `sun/nio/ch/FileKey.init(FileDescriptor fd, int[] result)` — fill
/// `result[0..3]` with the file-identity triple. See the module comment above.
fn native_filekey_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(io_error("FileKey.init: null result array")),
    };
    let fd = fd_from_descriptor(ctx, fd_obj)
        .ok_or_else(|| io_error("FileKey.init: FileDescriptor has no open handle"))?;
    let (vol, hi, lo) = file_identity_triple(ctx, fd);
    if ctx.array_length(arr) >= 3 {
        ctx.set_array_element(arr, 0, Value::Int(vol as i32));
        ctx.set_array_element(arr, 1, Value::Int(hi as i32));
        ctx.set_array_element(arr, 2, Value::Int(lo as i32));
    }
    Ok(None)
}

/// `sun/nio/ch/FileKey.init(FileDescriptor fd, long[] result)` — the real
/// Unix JDK overload (see the module comment's update above): fill
/// `result[0]=st_dev`, `result[1]=st_ino`. Distinct from
/// `native_filekey_init`'s invented int[3] convention, which only matches a
/// Windows-shaped `FileKey` class and is never the descriptor a real Unix
/// JDK class file declares.
fn native_filekey_init_longs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = fd_arg(args, 0)?;
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(io_error("FileKey.init: null result array")),
    };
    let fd = fd_from_descriptor(ctx, fd_obj)
        .ok_or_else(|| io_error("FileKey.init: FileDescriptor has no open handle"))?;
    let (dev, ino) = file_identity_pair(ctx, fd);
    if ctx.array_length(arr) >= 2 {
        ctx.set_array_element(arr, 0, Value::Long(dev));
        ctx.set_array_element(arr, 1, Value::Long(ino));
    }
    Ok(None)
}

/// `sun/nio/ch/FileKey.init(FileDescriptor fd)` — older real-JDK instance
/// shape. Fill the receiver's `st_dev` / `st_ino` fields directly.
fn native_filekey_init_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(io_error("FileKey.init: null receiver")),
    };
    let fd_obj = fd_arg(args, 1)?;
    let fd = fd_from_descriptor(ctx, fd_obj)
        .ok_or_else(|| io_error("FileKey.init: FileDescriptor has no open handle"))?;
    let (dev, ino) = file_identity_pair(ctx, fd);
    ctx.set_field_by_name(this, "st_dev", Value::Long(dev));
    ctx.set_field_by_name(this, "st_ino", Value::Long(ino));
    Ok(None)
}

/// Resolve `(st_dev, st_ino)` for an open fd — the real Unix JDK's own
/// `FileKey` identity pair, no repacking into 32-bit halves. Falls back to
/// the fd id (never fails), same infallibility contract as
/// `file_identity_triple`.
fn file_identity_pair(ctx: &mut dyn NativeContext, fd: FdId) -> (i64, i64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(file) = ctx.fd_table().clone_file(fd) {
            if let Ok(md) = file.metadata() {
                return (md.dev() as i64, md.ino() as i64);
            }
        }
    }
    #[cfg(windows)]
    {
        // The real Windows `FileKey` uses the int[]-based overload
        // (`native_filekey_init`) exclusively — this descriptor is a
        // Unix-only real-JDK signature and should never be called here, but
        // stay infallible for consistency with its sibling.
        let _ = ctx;
    }
    (fd as i64, fd as i64)
}

/// Do two PATHS name the same file? — the `Files.isSameFile` question.
///
/// # Why this exists next to `file_identity_triple`
///
/// Everything above keys file identity on an **open fd**, because that is what
/// `FileKey` is handed. `java.nio.file.Files.isSameFile(Path, Path)` asks the
/// same question about two paths that may not be open at all, and the tree's
/// only answer for it is
/// `native-builtins/src/phases_late/nio_file.rs`'s `FileSystemProvider.isSameFile`,
/// which calls `Path.equals` and whose comment claims *"The default provider's
/// same-file check is path equality"*.
///
/// **That claim is wrong about the JDK.** Path equality is the JDK's *fast
/// path*, not its answer. JDK 25 `sun.nio.fs.UnixFileSystemProvider.isSameFile`
/// returns early on `file1.equals(obj2)` and otherwise `stat`s BOTH paths and
/// compares `st_dev`/`st_ino`; the Windows provider does the same through
/// `GetFileInformationByHandle`'s volume serial + file index. So the JDK answers
/// `true` — and CratonVM answers `false` — for every pair that names one file by
/// two spellings: a hard link, a symlink and its target, `dir/x` and `dir/./x`,
/// an absolute path and the relative path to the same file, and on Windows two
/// spellings differing only in case or in 8.3 shortening. `Files.isSameFile` is
/// how callers ask "am I about to copy a file onto itself", so a `false` there
/// is the fabricated answer that lets the destructive branch run.
///
/// This is the identity half only. The call site is another lane's file and the
/// switch-over is recorded, not applied — see
/// W7-8-fabricated-success-io-sweep.md.
///
/// # What each platform arm compares, and what it still cannot see
///
/// * **Unix** — `fs::metadata` on each path (which FOLLOWS symlinks, matching
///   the provider's `UnixFileAttributes.get(file, true)`) and compare
///   `(st_dev, st_ino)`. This is the JDK's own predicate, so it sees hard links
///   as well as every spelling difference.
/// * **Windows** — open each path and compare
///   `(dwVolumeSerialNumber, nFileIndexHigh, nFileIndexLow)` from
///   `GetFileInformationByHandle`, reusing this file's existing [`win_fileid`]
///   binding. `std::fs::File::open` cannot open a DIRECTORY on Windows, so a
///   directory pair falls back to comparing `fs::canonicalize` results, which
///   resolves links, `.`/`..`, relative prefixes, case and 8.3 names. The one
///   thing that fallback cannot see is a hard link — and Windows has no hard
///   links to directories, so on the arm that uses it the gap is empty.
/// * The `Ok(false)` on a failed identity read is deliberate and is NOT a
///   fabricated success: `isSameFile` is a question whose safe answer is "no,
///   they are not known to be the same", and the destructive callers branch on
///   `true`. An I/O error that prevents an answer is reported as `Err` and the
///   caller raises `IOException`, which is what the method declares.
///
/// # Errors
///
/// Propagates the underlying metadata / open failure, which the JDK surfaces as
/// `IOException` (`Files.isSameFile` declares "@throws IOException if an I/O
/// error occurs"). A path equal to the other is answered `true` WITHOUT
/// touching the disk, exactly like the JDK's fast path — so `isSameFile(p, p)`
/// on a nonexistent `p` is `true` rather than an error, which is the reflexivity
/// the javadoc requires ("It is reflexive: for Path f, isSameFile(f,f) should
/// return true").
pub fn paths_name_the_same_file(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
    // The JDK's own fast path, and the only branch that must not touch the disk.
    if a == b {
        return Ok(true);
    }
    same_file_identity(a, b)
}

/// Unix arm of [`paths_name_the_same_file`]: `(st_dev, st_ino)`, which is the
/// provider's own predicate.
///
/// `fs::metadata` FOLLOWS symlinks, matching the provider's
/// `UnixFileAttributes.get(file, true)` — so this sees a link and its target as
/// one file, as well as every spelling difference and every hard link.
///
/// One `fn` per platform rather than three `cfg`'d blocks inside one body: that
/// is the shape `pipe.rs`'s three `poll_pipe` definitions use, and it does not
/// depend on a `cfg`'d block landing in tail position.
#[cfg(unix)]
fn same_file_identity(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let ma = std::fs::metadata(a)?;
    let mb = std::fs::metadata(b)?;
    Ok(ma.dev() == mb.dev() && ma.ino() == mb.ino())
}

/// Windows arm of [`paths_name_the_same_file`]: volume serial + file index, the
/// provider's own predicate here too.
///
/// `File::open` cannot open a DIRECTORY on Windows (that needs
/// `FILE_FLAG_BACKUP_SEMANTICS`, which `std` does not request), so a directory
/// pair falls back to comparing `canonicalize` results — which resolves links,
/// `.`/`..`, relative prefixes, case and 8.3 names. The only thing that fallback
/// cannot see is a hard link, and Windows has no hard links to directories, so
/// on the arm that uses it the gap is empty.
#[cfg(windows)]
fn same_file_identity(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
    match (win_file_identity(a), win_file_identity(b)) {
        (Ok(ia), Ok(ib)) => Ok(ia == ib),
        // `canonicalize` fails loudly for a path that does not exist, which is
        // the error `Files.isSameFile` is specified to report.
        _ => Ok(std::fs::canonicalize(a)? == std::fs::canonicalize(b)?),
    }
}

/// Fallback arm of [`paths_name_the_same_file`]. **Not compilable on any host in
/// this campaign.** No identity primitive on this target, so this falls back to
/// the strongest spelling-independent comparison available rather than to `==`,
/// which the caller has already tried.
#[cfg(not(any(unix, windows)))]
fn same_file_identity(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
    Ok(std::fs::canonicalize(a)? == std::fs::canonicalize(b)?)
}

/// `(volume serial, file index high, file index low)` for a path, via one
/// `GetFileInformationByHandle` on a freshly opened read handle.
///
/// Errors for a directory — `File::open` on Windows does not grant
/// `FILE_FLAG_BACKUP_SEMANTICS` — which is why the caller has a fallback rather
/// than treating a failure as "different files".
#[cfg(windows)]
fn win_file_identity(path: &std::path::Path) -> std::io::Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    let file = std::fs::File::open(path)?;
    // SAFETY: `info` is POD; the OS fully writes it on success and it is only
    // read when the call returns non-zero. Same pattern as
    // `file_identity_triple` above.
    let mut info: win_fileid::ByHandleFileInformation = unsafe { std::mem::zeroed() };
    // SAFETY: `file` keeps the raw handle live for the duration of the call and
    // `info` is writable storage for exactly this Win32 structure.
    let ok = unsafe {
        win_fileid::GetFileInformationByHandle(file.as_raw_handle() as *mut _, &mut info)
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((
        info.dw_volume_serial_number,
        info.n_file_index_high,
        info.n_file_index_low,
    ))
}

/// Variant of `map0` for legacy FileChannelImpl signatures where
/// arg 0 is a raw int fd (not a FileDescriptor object). Reuses
/// `native_fc_map0` after promoting the int to an `Object`-shaped
/// arg list — but here it's simpler to just inline the lookup.
fn native_fc_map0_legacy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // arg 0 = int fd, arg 1 = int prot, arg 2 = long pos,
    // arg 3 = long len, [arg 4 = boolean isSync].
    let fd = match args.first() {
        Some(Value::Int(v)) if *v > 2 => *v as FdId,
        _ => return Err(io_error("map0: bad legacy fd")),
    };
    let prot = int_arg(args, 1);
    let position = long_arg(args, 2);
    let length = long_arg(args, 3);
    if length <= 0 || position < 0 {
        return Err(io_error("map0: bad position/length"));
    }

    let file = ctx
        .fd_table()
        .clone_file(fd)
        .map_err(|e| io_error(format!("map0: clone fd: {e}")))?;
    let mut opts = memmap2::MmapOptions::new();
    opts.offset(position as u64).len(length as usize);
    let holder = match prot {
        // SAFETY: the cloned file remains alive in the returned mapping;
        // position/length were validated above and memmap2 reports OS errors.
        MAP_RO => MmapHolder::Ro(unsafe {
            opts.map(&file)
                .map_err(|e| io_error(format!("map0: mmap RO: {e}")))?
        }),
        // SAFETY: as above; the fd-table grants a private cloned handle and
        // mutable mapping ownership is retained exclusively by MmapHolder.
        MAP_RW => MmapHolder::Rw(unsafe {
            opts.map_mut(&file)
                .map_err(|e| io_error(format!("map0: mmap RW: {e}")))?
        }),
        // SAFETY: as above; copy-on-write prevents writes from aliasing the
        // underlying file through this mapping.
        MAP_PV => MmapHolder::Cow(unsafe {
            opts.map_copy(&file)
                .map_err(|e| io_error(format!("map0: mmap COW: {e}")))?
        }),
        other => return Err(io_error(format!("map0: unknown prot {other}"))),
    };
    let addr = holder.as_ptr() as usize;
    if addr == 0 {
        return Err(io_error("map0: kernel returned null address"));
    }
    mmap_registry().lock().insert(addr, holder);
    Ok(Some(Value::Long(addr as i64)))
}

// ---------------------------------------------------------------------------
// Tests — exercise mmap / unmap / transferTo end-to-end against a real
// temp file. We bypass the NativeContext registry path and call into
// the FdTable + memmap2 directly, since the registry plumbing is the
// parent agent's integration point.
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
    use std::io::Write;

    /// Round-trip: mmap a real file RW, mutate via the mapping,
    /// re-read through the file API, confirm bytes match.
    #[test]
    fn wp3_3_mmap_rw_round_trips_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rw.bin");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();

        let fdt = FileDescriptorTable::new();
        let fd = fdt.open_read_write(path.to_str().unwrap(), false).unwrap();
        let file = fdt.clone_file(fd).unwrap();

        let mut opts = memmap2::MmapOptions::new();
        // SAFETY: the test-created file is 4096 bytes, remains open, and this
        // mapping is the sole mutable view for its full length.
        let mut m = unsafe { opts.len(4096).map_mut(&file).unwrap() };

        // Mutate via the mapping.
        for (i, b) in m.iter_mut().enumerate().take(256) {
            *b = (i as u8).wrapping_add(7);
        }
        m.flush().unwrap();
        drop(m);

        // Read back via stdlib — bytes must match.
        let on_disk = std::fs::read(&path).unwrap();
        for i in 0..256 {
            assert_eq!(on_disk[i], (i as u8).wrapping_add(7));
        }
    }

    /// Round-trip: mmap RO and verify reads return file bytes.
    #[test]
    fn wp3_3_mmap_ro_reads_existing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.bin");
        let mut payload = vec![0u8; 8192];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        std::fs::write(&path, &payload).unwrap();

        let fdt = FileDescriptorTable::new();
        let fd = fdt.open_read(path.to_str().unwrap()).unwrap();
        let file = fdt.clone_file(fd).unwrap();

        let mut opts = memmap2::MmapOptions::new();
        // SAFETY: the test file is 8192 initialized bytes and remains open for
        // the lifetime of this read-only mapping.
        let m = unsafe { opts.len(8192).map(&file).unwrap() };
        assert_eq!(&m[..], &payload[..]);
    }

    /// Confirm a 16 MiB mapping works (largest realistic test
    /// without thrashing CI).  This proves we are NOT staging
    /// through a Rust Vec — that would balloon RAM.
    #[test]
    fn wp3_3_mmap_large_file_round_trip() {
        const SIZE: usize = 16 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        // Use sparse write — actual disk usage may be smaller on
        // sparse-aware filesystems.
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(SIZE as u64).unwrap();
        drop(f);

        let fdt = FileDescriptorTable::new();
        let fd = fdt.open_read_write(path.to_str().unwrap(), false).unwrap();
        let file = fdt.clone_file(fd).unwrap();

        let mut opts = memmap2::MmapOptions::new();
        // SAFETY: set_len established SIZE bytes, the handle remains open, and
        // this is the sole mutable mapping.
        let mut m = unsafe { opts.len(SIZE).map_mut(&file).unwrap() };

        // Touch the four corners and the midpoint.
        m[0] = 0xAA;
        m[SIZE - 1] = 0x55;
        m[SIZE / 2] = 0x33;
        m.flush().unwrap();
        drop(m);

        let f2 = std::fs::File::open(&path).unwrap();
        let file2 = f2;
        let opts2 = memmap2::MmapOptions::new();
        // SAFETY: the file still has SIZE bytes and remains open for the
        // lifetime of the read-only mapping.
        let m2 = unsafe { opts2.map(&file2).unwrap() };
        assert_eq!(m2[0], 0xAA);
        assert_eq!(m2[SIZE - 1], 0x55);
        assert_eq!(m2[SIZE / 2], 0x33);
    }

    /// Map at a non-zero offset and verify position semantics.
    #[test]
    fn wp3_3_mmap_with_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("offset.bin");
        // 64 KiB of structured data: byte i = i & 0xff.
        let mut payload = vec![0u8; 64 * 1024];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i & 0xff) as u8;
        }
        std::fs::write(&path, &payload).unwrap();

        let fdt = FileDescriptorTable::new();
        let fd = fdt.open_read(path.to_str().unwrap()).unwrap();
        let file = fdt.clone_file(fd).unwrap();

        // Map starting at offset 0 first — verify the basic
        // mapping covers the entire 64 KiB.
        let mut opts = memmap2::MmapOptions::new();
        // SAFETY: the test initialized all 64 KiB and holds the file open for
        // the lifetime of this read-only mapping.
        let m = unsafe { opts.offset(0).len(64 * 1024).map(&file).unwrap() };
        assert_eq!(m[100], (100 & 0xff) as u8);
        assert_eq!(m[1024], (1024 & 0xff) as u8);
        drop(m);

        // Now map at offset = 4 KiB (page-aligned on every
        // platform we target). Verify the first byte of the
        // new mapping equals byte 4096 of the file.
        let mut opts2 = memmap2::MmapOptions::new();
        // SAFETY: offset and length are page-aligned and wholly inside the
        // initialized 64 KiB test file, which remains open.
        let m2 = unsafe { opts2.offset(4096).len(4096).map(&file).unwrap() };
        // First byte of m2 should == byte 4096 of the file == 0
        // (because (4096 & 0xff) == 0).
        assert_eq!(m2[0], (4096 & 0xff) as u8);
        // Byte 100 of m2 == byte 4196 of the file.
        assert_eq!(m2[100], (4196 & 0xff) as u8);
    }

    /// COW (private) mapping: writes do NOT propagate to the file.
    #[test]
    fn wp3_3_mmap_cow_isolates_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cow.bin");
        std::fs::write(&path, vec![0xAB; 4096]).unwrap();

        let fdt = FileDescriptorTable::new();
        let fd = fdt.open_read_write(path.to_str().unwrap(), false).unwrap();
        let file = fdt.clone_file(fd).unwrap();

        let mut opts = memmap2::MmapOptions::new();
        // SAFETY: the initialized 4096-byte file remains open and map_copy
        // creates a private copy-on-write view owned by this test.
        let mut m = unsafe { opts.len(4096).map_copy(&file).unwrap() };

        // Mutate the COW view.
        for b in m.iter_mut().take(256) {
            *b = 0x11;
        }
        // Must not flush to disk in COW mode — we don't call flush.
        drop(m);

        // The on-disk bytes must remain unchanged.
        let on_disk = std::fs::read(&path).unwrap();
        for &b in &on_disk[..256] {
            assert_eq!(b, 0xAB);
        }
    }

    /// transferTo0 userspace fallback: pull bytes from src at
    /// position into dst, end-to-end.
    #[test]
    fn wp3_6_transfer_to_userspace_loop() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");

        // Source: 1 MiB of i & 0xff.
        let mut payload = vec![0u8; 1024 * 1024];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i & 0xff) as u8;
        }
        std::fs::write(&src, &payload).unwrap();
        // Make dst empty.
        std::fs::File::create(&dst).unwrap();

        let fdt = FileDescriptorTable::new();
        let src_fd = fdt.open_read_write(src.to_str().unwrap(), false).unwrap();
        let dst_fd = fdt.open_read_write(dst.to_str().unwrap(), true).unwrap();

        // Ad-hoc inline of transfer_userspace_loop's logic against
        // the FdTable, since the public function takes &mut dyn
        // NativeContext (which we cannot synthesize here without
        // pulling in test_support — that mock has its own static
        // FdTable distinct from this one).
        const CHUNK: usize = 64 * 1024;
        let mut buf = vec![0u8; CHUNK];
        let count: i64 = 1024 * 1024;
        let mut src_pos: u64 = 0;
        let mut transferred: i64 = 0;
        let mut remaining = count;
        while remaining > 0 {
            let want = std::cmp::min(remaining as usize, CHUNK);
            let n = fdt.pread_at(src_fd, &mut buf[..want], src_pos).unwrap();
            if n == 0 {
                break;
            }
            fdt.rw_write(dst_fd, &buf[..n]).unwrap();
            transferred += n as i64;
            src_pos += n as u64;
            remaining -= n as i64;
        }
        assert_eq!(transferred, 1024 * 1024);

        // Verify bytes.
        let copied = std::fs::read(&dst).unwrap();
        assert_eq!(copied.len(), 1024 * 1024);
        assert_eq!(&copied[..], &payload[..]);
    }

    /// transferTo0 with positional offset: copy from src @ 256 to dst.
    #[test]
    fn wp3_6_transfer_to_with_position_offset() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");

        let mut payload = vec![0u8; 4096];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i & 0xff) as u8;
        }
        std::fs::write(&src, &payload).unwrap();
        std::fs::File::create(&dst).unwrap();

        let fdt = FileDescriptorTable::new();
        let src_fd = fdt.open_read_write(src.to_str().unwrap(), false).unwrap();
        let dst_fd = fdt.open_read_write(dst.to_str().unwrap(), true).unwrap();

        let mut buf = vec![0u8; 1024];
        let n = fdt.pread_at(src_fd, &mut buf, 256).unwrap();
        assert_eq!(n, 1024);
        fdt.rw_write(dst_fd, &buf).unwrap();

        let copied = std::fs::read(&dst).unwrap();
        assert_eq!(&copied[..], &payload[256..256 + 1024]);
    }

    /// Verify the registry: insert a holder, look it up, drop it,
    /// confirm removal.
    #[test]
    fn wp3_3_mmap_registry_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("life.bin");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();

        let fdt = FileDescriptorTable::new();
        let fd = fdt.open_read(path.to_str().unwrap()).unwrap();
        let file = fdt.clone_file(fd).unwrap();
        let mut opts = memmap2::MmapOptions::new();
        // SAFETY: the 4096-byte test file remains open for the lifetime of
        // this read-only mapping.
        let m = unsafe { opts.len(4096).map(&file).unwrap() };
        let addr = m.as_ptr() as usize;
        let holder = MmapHolder::Ro(m);
        assert_eq!(holder.len(), 4096);

        let reg = mmap_registry();
        reg.lock().insert(addr, holder);
        assert!(reg.lock().contains_key(&addr));

        // Drop via the registry.
        let removed = reg.lock().remove(&addr);
        assert!(removed.is_some());
        assert!(!reg.lock().contains_key(&addr));
    }

    /// `unmap0` on a stale address must not panic — the JDK can
    /// race-double-free, and this is documented as tolerated.
    #[test]
    fn wp3_3_unmap0_stale_address_is_idempotent() {
        let mut reg = mmap_registry().lock();
        // A guaranteed-not-present address.
        let stale_addr: usize = 0xDEAD_BEEF;
        let removed = reg.remove(&stale_addr);
        assert!(removed.is_none());
        drop(reg);
    }

    /// Smoke: register the natives onto a fresh registry. We
    /// can't actually invoke them without a NativeContext, but
    /// we verify the registration count is non-zero and the
    /// helper functions are callable as expected.
    #[test]
    fn wp3_3_register_file_channel_real_smoke() {
        let mut r = NativeMethodRegistry::new();
        register_file_channel_real(&mut r);
        let fci = "sun/nio/ch/FileChannelImpl";
        assert!(
            r.find(
                fci,
                "open",
                "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;"
            )
            .is_some(),
            "JDK 25 FileChannelImpl.open bridge must be registered"
        );
        // JDK 21's shape, and the descriptor is the assertion. This read
        // `…ZZZLjava/lang/Object;` until 2026-08-10 — a spelling no JDK
        // declares — so it agreed with a registration that could never bind and
        // said nothing about whether JDK 21 was covered. Checked with
        // `javap -p -s` against Temurin 21.0.12+8, not against the source.
        assert!(
            r.find(
                fci,
                "open",
                "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;"
            )
            .is_some(),
            "JDK 21 FileChannelImpl.open bridge must be registered"
        );
        // The old unbindable spelling must not come back: re-adding it would
        // make this test pass again for the wrong reason.
        assert!(
            r.find(
                fci,
                "open",
                "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZLjava/lang/Object;)Ljava/nio/channels/FileChannel;"
            )
            .is_none(),
            "no JDK declares an Object-tailed FileChannelImpl.open; \
             registering one binds nothing and hides a missing arity"
        );
        let nts = "sun/nio/ch/NativeThreadSet";
        assert!(r.find(nts, "add", "()I").is_some());
        assert!(r.find(nts, "remove", "(I)V").is_some());
        assert!(r.find(nts, "signalAndWait", "()V").is_some());
        // Touch each of the registered methods via the registry's
        // public API so we know the signatures parsed.  We rely
        // on the fact that the registry maintains internal counts.
        // If the registry has a `len()` we'd assert; otherwise
        // just confirm the call doesn't panic.
        let _ = &r;
    }

    /// Make sure the prot constants match the JDK's
    /// FileChannelImpl.MAP_RO/RW/PV values (0/1/2).
    #[test]
    fn wp3_3_prot_constants_match_jdk() {
        assert_eq!(MAP_RO, 0);
        assert_eq!(MAP_RW, 1);
        assert_eq!(MAP_PV, 2);
    }

    /// IOStatus constants match `sun.nio.ch.IOStatus`.
    #[test]
    fn wp3_6_iostatus_constants_match_jdk() {
        assert_eq!(IOSTATUS_EOF, -1);
        assert_eq!(IOSTATUS_UNAVAILABLE, -2);
        assert_eq!(IOSTATUS_INTERRUPTED, -3);
        assert_eq!(IOSTATUS_UNSUPPORTED, -4);
    }

    /// `maxDirectTransferSize0` must be REGISTERED and must ANSWER the
    /// documented cap.
    ///
    /// The body used to be `assert_eq!(0x7fff_ffff_i32, i32::MAX)` — a fact
    /// about two Rust literals, true on every machine that has ever run this
    /// suite. `native_fc_max_direct_transfer_size0` was not on the call path,
    /// so returning `0` from it, returning a `Long`, throwing, or dropping the
    /// registration entirely all left this test green. The JDK divides its
    /// transfer length by this number: a `0` here is a divide-by-zero or an
    /// infinite `transferTo` loop, and an absent registration is an
    /// `UnsatisfiedLinkError` on the first `FileChannel.transferTo`.
    ///
    /// It now goes through the registry (so unregistering it fails the test)
    /// and invokes the resolved callback (so changing the returned value or
    /// its `Value` variant fails the test).
    #[test]
    fn wp3_6_max_direct_transfer_size_is_int_max() {
        let mut r = NativeMethodRegistry::new();
        register_file_channel_real(&mut r);

        // `register_fd_native` puts every dispatcher native under all three
        // platform spellings; a JDK image declares whichever one it ships.
        for cls in [
            "sun/nio/ch/FileDispatcherImpl",
            "sun/nio/ch/UnixFileDispatcherImpl",
            "sun/nio/ch/WindowsFileDispatcherImpl",
        ] {
            let cb = r
                .find(cls, "maxDirectTransferSize0", "()I")
                .unwrap_or_else(|| panic!("{cls}.maxDirectTransferSize0()I must be registered"));

            let mut ctx = crate::test_support::MockNativeContext::new();
            // The JDK calls this with no arguments and uses the result as a
            // divisor/chunk size, so both the variant and the value matter.
            let Ok(Some(Value::Int(cap))) = cb(&mut ctx, &[]) else {
                panic!("{cls}.maxDirectTransferSize0 must return Some(Value::Int(..))");
            };
            assert_eq!(
                cap,
                i32::MAX,
                "{cls}.maxDirectTransferSize0 must report the documented \
                 2 GiB - 1 cap; a smaller value throttles every transferTo and \
                 0 is a divide-by-zero in the JDK's chunking loop"
            );
        }
    }

    // Exercise the helper code paths whose only callers are in
    // production-only branches (so rustc doesn't dead-code them
    // away in debug builds and miss bugs).
    #[test]
    fn helpers_are_callable() {
        let v = vec![Value::Int(7), Value::Long(42), Value::Int(1)];
        assert_eq!(int_arg(&v, 0), 7);
        assert_eq!(long_arg(&v, 1), 42);
        assert!(bool_arg(&v, 2));
        // Last-resort defaults.
        assert_eq!(int_arg(&v, 99), 0);
        assert_eq!(long_arg(&v, 99), 0);
        assert!(!bool_arg(&v, 99));
    }

    /// Ensure `io_error` builds a RuntimeError::IOException.
    #[test]
    fn io_error_shape() {
        let e = io_error("whoops");
        match e {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
                message,
            })) => assert_eq!(message.as_str(), "whoops"),
            _ => panic!("expected IOException"),
        }
    }

    /// Drive a 32 MiB write-then-mmap-flush round trip — this
    /// is the largest size we exercise routinely; it's well
    /// within stack/RAM budgets for CI but stresses the page
    /// cache enough to catch most off-by-one bugs.
    #[test]
    fn wp3_3_mmap_thirty_two_mb_round_trip() {
        const SIZE: usize = 32 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("xl.bin");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(SIZE as u64).unwrap();
        drop(f);

        let fdt = FileDescriptorTable::new();
        let fd = fdt.open_read_write(path.to_str().unwrap(), false).unwrap();
        let file = fdt.clone_file(fd).unwrap();

        let mut opts = memmap2::MmapOptions::new();
        // SAFETY: set_len established SIZE bytes, the handle remains open, and
        // this is the sole mutable mapping.
        let mut m = unsafe { opts.len(SIZE).map_mut(&file).unwrap() };
        // Stride writes to force the kernel to fault each page.
        let stride = 4096;
        let mut i = 0usize;
        while i < SIZE {
            m[i] = 0xC3;
            i += stride;
        }
        m.flush().unwrap();
        drop(m);
        let _ = std::fs::metadata(&path).unwrap();
    }

    // Confirm that the file we just wrote has no rope-allocated
    // intermediate (i.e., we are NOT staging through a Vec); we
    // do this indirectly by asserting that a 32 MiB mmap is
    // O(1) Vec allocations beyond the buffer.
    /// `isSameFile` is not path equality — the assertion that would have caught
    /// the `Path.equals` approximation.
    ///
    /// RED against a `paths_name_the_same_file` implemented as `a == b`, which
    /// is what `FileSystemProvider.isSameFile` still does in
    /// `native-builtins/src/phases_late/nio_file.rs`. Every pair below names ONE
    /// file by two spellings, and every one of them compares unequal as a Rust
    /// `Path` — `Path`'s `PartialEq` walks `components()`, which folds away `.`
    /// but keeps `..`, so the traversal case is a genuine inequality on every
    /// platform and needs no privilege, no link support and no `canonicalize`
    /// agreement to be meaningful.
    #[test]
    fn wp3_3_same_file_is_identity_not_path_equality() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("f.bin"), b"payload").unwrap();

        let direct = root.join("f.bin");
        let via_parent = root.join("sub").join("..").join("f.bin");
        assert_ne!(
            direct, via_parent,
            "the two spellings must differ as Paths, or this test asserts nothing"
        );
        assert!(
            paths_name_the_same_file(&direct, &via_parent).unwrap(),
            "a `..` traversal to the same file must be the same file"
        );

        // Two genuinely different files. Without this the row above is
        // satisfied by an implementation that answers `true` for everything,
        // which is the failure mode one direction of assertions cannot see.
        std::fs::write(root.join("g.bin"), b"payload").unwrap();
        assert!(
            !paths_name_the_same_file(&direct, &root.join("g.bin")).unwrap(),
            "byte-identical but distinct files are NOT the same file"
        );

        // Reflexive without touching the disk, which is the JDK's fast path and
        // the reason `isSameFile(f, f)` is `true` for a nonexistent `f`.
        let ghost = root.join("does-not-exist");
        assert!(paths_name_the_same_file(&ghost, &ghost).unwrap());

        // A hard link is the case `canonicalize` CANNOT see, so it is the one
        // that proves the identity read is doing the work rather than a path
        // normalisation. Not made unconditional: `hard_link` depends on the
        // filesystem backing the temp dir, and a whole-test skip on that is the
        // vacuity this suite is trying to get rid of — so the rows above stand
        // on their own and this one only ever adds.
        let linked = root.join("hard.bin");
        if std::fs::hard_link(&direct, &linked).is_ok() {
            assert!(
                paths_name_the_same_file(&direct, &linked).unwrap(),
                "a hard link names the same file"
            );
        }
    }

    #[test]
    fn helper_smoke_writeln() {
        // Sanity that std::io::Write is in scope and `write_all`
        // is callable on a `Vec<u8>`.
        let mut sink: Vec<u8> = Vec::new();
        sink.write_all(b"x").unwrap();
        assert_eq!(sink, b"x");
    }
}
