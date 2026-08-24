// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `sun.nio.ch.FileChannelImpl.read/write(ByteBuffer)`, `position()`,
//! `position(long)` and `size()` — the JDK glue chain around each, collapsed
//! into one native call.
//!
//! # The measurement this exists for
//!
//! `performance/filechannel-heap-read-glue-depth-FIXED-20260823.md`
//! priced `FileChannel.read` into a HEAP buffer at **~8.7x HotSpot**
//! (`probes/FileChannelHeapReadProbe.java`, 20 000 reads of 24 bytes). The
//! cause is not an allocation and not the temporary-direct-buffer cache —
//! both were measured and refuted there. It is that one `read(ByteBuffer)`
//! walks ~20 JDK frames:
//!
//! ```text
//! read -> implRead -> ensureOpen -> beginBlocking -> begin -> blockedOn
//!      -> IOUtil.read -> getTemporaryDirectBuffer -> readIntoNativeBuffer
//!      -> acquireScope -> bufferAddress -> FileDispatcherImpl.read -> read0
//!      -> releaseScope -> flip -> HeapByteBuffer.put -> endBlocking -> end
//! ```
//!
//! C2 inlines that chain into a handful of instructions around one syscall.
//! CratonVM runs it. Nothing in the profile is above 11% — it is a
//! distribution, so the only change that moves it is removing the chain.
//!
//! # Why this is safe, stated as the refusals rather than as a claim
//!
//! The parent page's reason for NOT taking this fix was that a fast path has
//! to reproduce the position advance, the `IOStatus.INTERRUPTED` retry,
//! `IOStatus.normalize`, the EOF-is-`-1` convention, the read-only and
//! non-readable refusals, and the `beginBlocking`/`endBlocking` pairing that
//! makes the channel closeable by another thread mid-read — and that getting
//! any one wrong silently desynchronises the channel position.
//!
//! This file answers that by **refusing** every shape it does not model, and
//! refusing means re-entering the SAME method as pure bytecode, with the
//! arguments it was given, so a refusal is bit-for-bit the un-intercepted VM.
//! The fast path runs only when ALL of these hold:
//!
//! * `jfrTracing` is false — otherwise `read(ByteBuffer)`'s own body would
//!   have taken its `traceImplRead` branch and emitted an event.
//! * the channel is open, `readable` (resp. `writable`), and not `direct`
//!   (no O_DIRECT alignment rules to honour).
//! * the buffer is a plain heap `ByteBuffer`: `hb != null`, `segment == null`
//!   (no `MemorySegment` scope to acquire), and for a read, not read-only.
//! * for a WRITE, the descriptor is not in append mode — `write0` seeks to
//!   end for one and this path writes at the current position.
//! * `interruptedTarget` is null and the calling thread's interrupt flag is
//!   clear, so `begin()` would not have performed an asynchronous close.
//! * the calling thread is a PLATFORM thread. `VirtualThread.blockedOn`
//!   wraps its superclass body in `disableSuspendAndPreempt` /
//!   `enableSuspendAndPreempt`, and a fast path that published the blocker
//!   without them would leave a virtual thread preemptable between the
//!   publication and the I/O.
//!
//! Everything else is reproduced exactly rather than skipped:
//!
//! * `positionLock` is held across the whole operation, so a concurrent
//!   `position()`/`read`/`write` on the same channel serialises as it does
//!   under the JDK's `synchronized (positionLock)`.
//!   Reading `this`/`dst`/`hb` after that acquire is safe for the reason the
//!   other ~80 native `monitor_enter` sites rely on and `monitor_enter_gc_safe`
//!   spells out: the plain contended path leaves the caller counted in an
//!   in-flight STW barrier's `expected` set rather than being excused from it,
//!   so no moving collection completes while this thread waits. A future
//!   change that switched this file to the GC-safe variant would have to
//!   pin-and-refresh every one of those references.
//! * `blockedOn(interruptor)` is published on the calling thread before the
//!   I/O and cleared after, so another thread's `Thread.interrupt()` during
//!   the call still reaches `AbstractInterruptibleChannel$1.interrupt` and
//!   closes the channel. Skipping it would have deferred an asynchronous
//!   close by one operation — a real weakening, and cheap to avoid: two
//!   monitor operations and one field write.
//! * `end(completed)`'s exceptional tail (`ClosedByInterruptException` when
//!   this thread was the interrupted target, `AsynchronousCloseException`
//!   when the channel closed under an incomplete operation) is delegated to
//!   the JDK's own `end` — but only on the rare branch that can throw, so the
//!   hot path never enters a Java frame for it.
//! * EOF is `-1` (`IOStatus.normalize` of `IOStatus.EOF`), an empty buffer is
//!   `0`, and the buffer's `position` advances by exactly the byte count.
//!
//! # The kill switch
//!
//! `CRATONVM_FC_FAST_IO=0` removes the registration entirely, so the "off"
//! arm is the un-intercepted VM rather than one that still answers part of
//! the surface natively. One binary, one bit of configuration — a
//! cross-binary A/B is not an A/B.
//!
//! `CRATONVM_FC_FAST_IO_STATS=1` prints the engagement census at exit. A
//! speedup quoted without it is unreadable: "the fast path ran" and "every
//! call refused while the host happened to be quieter" produce the same wall
//! clock.

use cratonvm_native_api::fd_table::FdId;
use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ClassId, ObjectRef, Value};

const FCI: &str = "sun/nio/ch/FileChannelImpl";

/// Is the fast path registered? See the module comment — this gates the
/// REGISTRATION, not each call.
fn engaged() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_FC_FAST_IO")
            .ok()
            .as_deref()
            != Some("0")
    })
}

pub mod stats {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static READ_FAST: AtomicU64 = AtomicU64::new(0);
    pub static READ_REFUSED: AtomicU64 = AtomicU64::new(0);
    pub static WRITE_FAST: AtomicU64 = AtomicU64::new(0);
    pub static WRITE_REFUSED: AtomicU64 = AtomicU64::new(0);
    pub static POS_FAST: AtomicU64 = AtomicU64::new(0);
    pub static POS_REFUSED: AtomicU64 = AtomicU64::new(0);
    pub static SIZE_FAST: AtomicU64 = AtomicU64::new(0);
    pub static SIZE_REFUSED: AtomicU64 = AtomicU64::new(0);

    pub(super) fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }

    /// `true` when anything at all went through this file, so a caller can
    /// decide whether the census is worth printing.
    pub fn touched() -> bool {
        ALL.iter().any(|c| c.load(Ordering::Relaxed) != 0)
    }

    /// Every counter, in the order [`report`] prints them.
    static ALL: [&AtomicU64; 8] = [
        &READ_FAST,
        &READ_REFUSED,
        &WRITE_FAST,
        &WRITE_REFUSED,
        &POS_FAST,
        &POS_REFUSED,
        &SIZE_FAST,
        &SIZE_REFUSED,
    ];

    /// The census line. Both halves of every pair are printed, including the
    /// zeros: `fast=N refused=0` and a missing `refused` row are different
    /// claims, and only the first is readable.
    pub fn report() -> String {
        format!(
            "[cratonvm] filechannel fast I/O: read fast={} refused={}  write fast={} refused={}  \
             pos fast={} refused={}  size fast={} refused={}",
            READ_FAST.load(Ordering::Relaxed),
            READ_REFUSED.load(Ordering::Relaxed),
            WRITE_FAST.load(Ordering::Relaxed),
            WRITE_REFUSED.load(Ordering::Relaxed),
            POS_FAST.load(Ordering::Relaxed),
            POS_REFUSED.load(Ordering::Relaxed),
            SIZE_FAST.load(Ordering::Relaxed),
            SIZE_REFUSED.load(Ordering::Relaxed),
        )
    }
}

// ---------------------------------------------------------------------------
// Field-index memos
// ---------------------------------------------------------------------------

/// The `FileChannelImpl` slots the fast path reads.
///
/// Resolved by NAME once per class id and memoised, so a JDK that reorders its
/// fields moves this code's reads with it. `resolve_field_index_by_class_id`
/// takes a class-manager read lock, and this path runs twice per
/// `FileChannel.read`; a one-entry thread-local memo keeps that off the hot
/// path without a global lock of its own.
#[derive(Clone, Copy)]
struct ChannelFields {
    class_id: ClassId,
    fd: usize,
    readable: usize,
    writable: usize,
    direct: usize,
    closed: usize,
    position_lock: usize,
    interruptor: usize,
    interrupted_target: usize,
}

/// The `ByteBuffer` slots the fast path reads. `position`/`limit` are private
/// to `java.nio.Buffer`; `hb`/`offset`/`isReadOnly` are package-private on
/// `java.nio.ByteBuffer`; `segment` is `java.nio.Buffer`'s and is non-null
/// exactly for the `MemorySegment`-backed buffers this file refuses.
#[derive(Clone, Copy)]
struct BufferFields {
    class_id: ClassId,
    position: usize,
    limit: usize,
    hb: usize,
    offset: usize,
    is_read_only: usize,
    segment: usize,
}

thread_local! {
    static CHANNEL_MEMO: std::cell::Cell<Option<ChannelFields>> = const { std::cell::Cell::new(None) };
    static BUFFER_MEMO: std::cell::Cell<Option<BufferFields>> = const { std::cell::Cell::new(None) };
}

fn channel_fields(ctx: &dyn NativeContext, class_id: ClassId) -> Option<ChannelFields> {
    if let Some(f) = CHANNEL_MEMO.with(|c| c.get()) {
        if f.class_id == class_id {
            return Some(f);
        }
    }
    let idx = |n: &str| ctx.resolve_field_index_by_class_id(class_id, n);
    let f = ChannelFields {
        class_id,
        fd: idx("fd")?,
        readable: idx("readable")?,
        writable: idx("writable")?,
        direct: idx("direct")?,
        closed: idx("closed")?,
        position_lock: idx("positionLock")?,
        interruptor: idx("interruptor")?,
        // JDK 25 renamed `interrupted` to `interruptedTarget`. Accept either,
        // and refuse the fast path entirely if neither is present rather than
        // guessing a slot: a wrong index here reads an unrelated field and
        // would silently skip the asynchronous-close check.
        interrupted_target: idx("interruptedTarget").or_else(|| idx("interrupted"))?,
    };
    CHANNEL_MEMO.with(|c| c.set(Some(f)));
    Some(f)
}

fn buffer_fields(ctx: &dyn NativeContext, class_id: ClassId) -> Option<BufferFields> {
    if let Some(f) = BUFFER_MEMO.with(|c| c.get()) {
        if f.class_id == class_id {
            return Some(f);
        }
    }
    let idx = |n: &str| ctx.resolve_field_index_by_class_id(class_id, n);
    let f = BufferFields {
        class_id,
        position: idx("position")?,
        limit: idx("limit")?,
        hb: idx("hb")?,
        offset: idx("offset")?,
        is_read_only: idx("isReadOnly")?,
        segment: idx("segment")?,
    };
    BUFFER_MEMO.with(|c| c.set(Some(f)));
    Some(f)
}

/// The `Thread` slots `blockedOn` writes. Same memo argument as above; the
/// thread class is one class for the whole VM.
#[derive(Clone, Copy)]
struct ThreadFields {
    class_id: ClassId,
    interrupt_lock: usize,
    nio_blocker: usize,
}

thread_local! {
    static THREAD_MEMO: std::cell::Cell<Option<ThreadFields>> = const { std::cell::Cell::new(None) };
}

fn thread_fields(ctx: &dyn NativeContext, class_id: ClassId) -> Option<ThreadFields> {
    if let Some(f) = THREAD_MEMO.with(|c| c.get()) {
        if f.class_id == class_id {
            return Some(f);
        }
    }
    let f = ThreadFields {
        class_id,
        interrupt_lock: ctx.resolve_field_index_by_class_id(class_id, "interruptLock")?,
        nio_blocker: ctx.resolve_field_index_by_class_id(class_id, "nioBlocker")?,
    };
    THREAD_MEMO.with(|c| c.set(Some(f)));
    Some(f)
}

// ---------------------------------------------------------------------------
// Small readers
// ---------------------------------------------------------------------------

fn obj_at(args: &[Value], i: usize) -> Option<ObjectRef> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn bool_field(ctx: &dyn NativeContext, obj: ObjectRef, slot: usize) -> bool {
    !matches!(ctx.get_field(obj, slot), Value::Int(0))
}

fn int_field(ctx: &dyn NativeContext, obj: ObjectRef, slot: usize) -> i32 {
    match ctx.get_field(obj, slot) {
        Value::Int(v) => v,
        Value::Long(v) => v as i32,
        _ => 0,
    }
}

fn ref_field(ctx: &dyn NativeContext, obj: ObjectRef, slot: usize) -> Option<ObjectRef> {
    match ctx.get_field(obj, slot) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// The `FdId` a prior `open0` stored on the `FileDescriptor` — Windows keeps
/// it in `handle` (a long), Unix in `fd` (an int). Same rule as
/// `nio_native::fd_from_descriptor`; duplicated rather than made public
/// because this file must not grow a dependency on that module's privates.
fn fd_from_descriptor(ctx: &dyn NativeContext, fd_obj: ObjectRef) -> Option<FdId> {
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

/// Is JFR's file-read/write tracing armed?
///
/// `FileChannelImpl.read(ByteBuffer)`'s own body is
/// `if (jfrTracing && FileReadEvent.enabled()) return traceImplRead(dst);`
/// — so a fast path that ignored this would silently drop an event the
/// un-intercepted VM emits. Read from the static slot every call (one array
/// index) rather than cached, because JFR can be started after the first read.
fn jfr_tracing(ctx: &dyn NativeContext, class_id: ClassId) -> bool {
    match ctx.static_field_index_by_name(class_id, "jfrTracing") {
        Some(i) => !matches!(ctx.get_static_field(class_id, i), Value::Int(0)),
        // No such static on this image: the branch cannot be taken.
        None => false,
    }
}

// ---------------------------------------------------------------------------
// blockedOn
// ---------------------------------------------------------------------------

/// `AbstractInterruptibleChannel.blockedOn(intr)` — `Thread.blockedOn`, which
/// is `synchronized (interruptLock) { nioBlocker = b; }`.
///
/// This is what makes another thread's `Thread.interrupt()` reach
/// `AbstractInterruptibleChannel$1.interrupt` and close the channel while this
/// one is inside the I/O. Two monitor operations and one field write.
fn blocked_on(ctx: &mut dyn NativeContext, thread: ObjectRef, tf: ThreadFields, v: Value) {
    if let Some(lock) = ref_field(ctx, thread, tf.interrupt_lock) {
        ctx.monitor_enter(lock);
        ctx.set_field(thread, tf.nio_blocker, v);
        ctx.monitor_exit(lock);
    } else {
        ctx.set_field(thread, tf.nio_blocker, v);
    }
}

// ---------------------------------------------------------------------------
// position() / position(long) / size()
// ---------------------------------------------------------------------------
//
// `read`/`write` were the headline, but they are not the whole cost of a
// `DataInput`-shaped reader. `FileChannelHeapReadProbe`'s loop calls
// `ch.position()` once per iteration, and with the two transfers collapsed the
// A/B still read 19.6 us against HotSpot's 2.2 — because `position()` walks
// its own `ensureOpen -> synchronized(positionLock) -> beginBlocking ->
// threads.add -> nd.seek -> IOStatus.normalize -> threads.remove ->
// endBlocking` chain. Same skeleton, same refusals, same `end` delegation as
// the transfers above; the only new question is `append`.

/// The channel state these three need, or `None` meaning "refuse".
///
/// Deliberately NOT the transfers' [`screen`]: there is no buffer here, and
/// the direction flag that matters is neither `readable` nor `writable` —
/// `position()` is legal on a channel opened for either.
struct PosState {
    cf: ChannelFields,
    tf: ThreadFields,
    thread: ObjectRef,
    position_lock: ObjectRef,
    interruptor: Value,
    fd: FdId,
    /// `fdAccess.getAppend(fd)`. `position()` answers `nd.size(fd)` rather
    /// than `nd.seek(fd, -1)` on an append-mode descriptor, because the OS
    /// position is not where the next write lands.
    append: bool,
}

fn screen_pos(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<PosState> {
    let channel_class = ctx.class_id_of_object(this);
    if jfr_tracing(ctx, channel_class) {
        return None;
    }
    let cf = channel_fields(ctx, channel_class)?;
    if bool_field(ctx, this, cf.closed) || ref_field(ctx, this, cf.interrupted_target).is_some() {
        return None;
    }
    if ctx.is_interrupted(false) || ctx.is_current_virtual() {
        return None;
    }
    let thread = ctx.current_thread_object();
    let tf = thread_fields(ctx, ctx.class_id_of_object(thread))?;
    let fd_obj = ref_field(ctx, this, cf.fd)?;
    let fd = fd_from_descriptor(ctx, fd_obj)?;
    // `FileDescriptor.append` is the field `JavaIOFileDescriptorAccess.getAppend`
    // reads. Absent on an image that spells it differently: refuse rather than
    // assume `false`, which would answer an append channel's position with the
    // OS cursor instead of the file size.
    let append = match ctx.get_field_by_name(fd_obj, "append") {
        Value::Int(v) => v != 0,
        _ => return None,
    };
    let position_lock = ref_field(ctx, this, cf.position_lock)?;
    let interruptor = ctx.get_field(this, cf.interruptor);
    Some(PosState {
        cf,
        tf,
        thread,
        position_lock,
        interruptor,
        fd,
        append,
    })
}

/// Hand a no-argument `long`-returning channel query back to its own bytecode.
fn refuse_impl_long(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    let mut full = Vec::with_capacity(args.len() + 1);
    full.push(Value::Object(Some(this)));
    full.extend_from_slice(args);
    ctx.invoke_special_bytecode_only(FCI, name, descriptor, &full)
}

fn native_fc_position(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = obj_at(args, 0) else {
        return Err(io_error("FileChannelImpl.position: null receiver"));
    };
    let Some(st) = screen_pos(ctx, this) else {
        stats::bump(&stats::POS_REFUSED);
        return refuse_impl_long(ctx, this, "position", "()J", &[]);
    };
    stats::bump(&stats::POS_FAST);

    ctx.monitor_enter(st.position_lock);
    blocked_on(ctx, st.thread, st.tf, st.interruptor);
    if bool_field(ctx, this, st.cf.closed) {
        blocked_on(ctx, st.thread, st.tf, Value::Object(None));
        ctx.monitor_exit(st.position_lock);
        finish(ctx, this, st.cf, false)?;
        return Ok(Some(Value::Long(0)));
    }
    let outcome = if st.append {
        ctx.fd_table()
            .file_size(st.fd)
            .map(|n| n as i64)
            .map_err(|e| io_error(format!("FileChannelImpl.position: {e}")))
    } else {
        ctx.fd_table()
            .rw_seek(st.fd, std::io::SeekFrom::Current(0))
            .map(|n| n as i64)
            .map_err(|e| io_error(format!("FileChannelImpl.position: {e}")))
    };
    blocked_on(ctx, st.thread, st.tf, Value::Object(None));
    ctx.monitor_exit(st.position_lock);

    let p = match outcome {
        Ok(p) => p,
        Err(e) => {
            finish(ctx, this, st.cf, false)?;
            return Err(e);
        }
    };
    finish(ctx, this, st.cf, p > -1)?;
    Ok(Some(Value::Long(p)))
}

fn native_fc_position_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = obj_at(args, 0) else {
        return Err(io_error("FileChannelImpl.position: null receiver"));
    };
    let new_position = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => -1,
    };
    // `IllegalArgumentException` for a negative argument, raised by the JDK's
    // own body with the JDK's own message.
    if new_position < 0 {
        stats::bump(&stats::POS_REFUSED);
        return refuse_impl_long(
            ctx,
            this,
            "position",
            "(J)Ljava/nio/channels/FileChannel;",
            &[Value::Long(new_position)],
        );
    }
    let Some(st) = screen_pos(ctx, this) else {
        stats::bump(&stats::POS_REFUSED);
        return refuse_impl_long(
            ctx,
            this,
            "position",
            "(J)Ljava/nio/channels/FileChannel;",
            &[Value::Long(new_position)],
        );
    };
    stats::bump(&stats::POS_FAST);

    ctx.monitor_enter(st.position_lock);
    blocked_on(ctx, st.thread, st.tf, st.interruptor);
    if bool_field(ctx, this, st.cf.closed) {
        blocked_on(ctx, st.thread, st.tf, Value::Object(None));
        ctx.monitor_exit(st.position_lock);
        finish(ctx, this, st.cf, false)?;
        // The JDK returns `null` from this branch, not `this`.
        return Ok(Some(Value::Object(None)));
    }
    let outcome = ctx
        .fd_table()
        .rw_seek(st.fd, std::io::SeekFrom::Start(new_position as u64))
        .map(|n| n as i64)
        .map_err(|e| io_error(format!("FileChannelImpl.position: {e}")));
    blocked_on(ctx, st.thread, st.tf, Value::Object(None));
    ctx.monitor_exit(st.position_lock);

    let p = match outcome {
        Ok(p) => p,
        Err(e) => {
            finish(ctx, this, st.cf, false)?;
            return Err(e);
        }
    };
    finish(ctx, this, st.cf, p > -1)?;
    Ok(Some(Value::Object(Some(this))))
}

fn native_fc_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = obj_at(args, 0) else {
        return Err(io_error("FileChannelImpl.size: null receiver"));
    };
    let Some(st) = screen_pos(ctx, this) else {
        stats::bump(&stats::SIZE_REFUSED);
        return refuse_impl_long(ctx, this, "size", "()J", &[]);
    };
    stats::bump(&stats::SIZE_FAST);

    ctx.monitor_enter(st.position_lock);
    blocked_on(ctx, st.thread, st.tf, st.interruptor);
    if bool_field(ctx, this, st.cf.closed) {
        blocked_on(ctx, st.thread, st.tf, Value::Object(None));
        ctx.monitor_exit(st.position_lock);
        finish(ctx, this, st.cf, false)?;
        return Ok(Some(Value::Long(-1)));
    }
    let outcome = ctx
        .fd_table()
        .file_size(st.fd)
        .map(|n| n as i64)
        .map_err(|e| io_error(format!("FileChannelImpl.size: {e}")));
    blocked_on(ctx, st.thread, st.tf, Value::Object(None));
    ctx.monitor_exit(st.position_lock);

    let s = match outcome {
        Ok(s) => s,
        Err(e) => {
            finish(ctx, this, st.cf, false)?;
            return Err(e);
        }
    };
    finish(ctx, this, st.cf, s > -1)?;
    Ok(Some(Value::Long(s)))
}

// ---------------------------------------------------------------------------
// read(ByteBuffer) / write(ByteBuffer)
// ---------------------------------------------------------------------------

/// Everything the fast path needs, or `None` meaning "refuse".
struct FastState {
    cf: ChannelFields,
    bf: BufferFields,
    tf: ThreadFields,
    thread: ObjectRef,
    position_lock: ObjectRef,
    interruptor: Value,
    fd: FdId,
    hb: ObjectRef,
    /// Byte-array index of the buffer's current position: `offset + position`.
    base: usize,
    pos: i32,
    rem: usize,
}

/// The shared screen for `read` and `write`. `want_readable` picks which of
/// the channel's two direction flags must be set; `for_read` additionally
/// refuses a read-only destination.
fn screen(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    dst: ObjectRef,
    for_read: bool,
) -> Option<FastState> {
    let channel_class = ctx.class_id_of_object(this);
    if jfr_tracing(ctx, channel_class) {
        return None;
    }
    let cf = channel_fields(ctx, channel_class)?;
    if bool_field(ctx, this, cf.closed)
        || bool_field(ctx, this, cf.direct)
        || ref_field(ctx, this, cf.interrupted_target).is_some()
    {
        return None;
    }
    let direction = if for_read { cf.readable } else { cf.writable };
    if !bool_field(ctx, this, direction) {
        return None;
    }

    let bf = buffer_fields(ctx, ctx.class_id_of_object(dst))?;
    // A `MemorySegment`-backed buffer has a scope that `IOUtil.acquireScope`
    // holds across the transfer; this file does not model it.
    if ref_field(ctx, dst, bf.segment).is_some() {
        return None;
    }
    let hb = ref_field(ctx, dst, bf.hb)?;
    if for_read && bool_field(ctx, dst, bf.is_read_only) {
        return None;
    }

    if ctx.is_interrupted(false) || ctx.is_current_virtual() {
        return None;
    }
    let thread = ctx.current_thread_object();
    let tf = thread_fields(ctx, ctx.class_id_of_object(thread))?;

    let fd_obj = ref_field(ctx, this, cf.fd)?;
    // An APPEND descriptor is refused, and only the WRITE direction needs it:
    // `FileDispatcherImpl.write` passes `append` down to `write0`, which on
    // Windows seeks to end before writing (on Linux the kernel does it from
    // `O_APPEND`). `FileDescriptorTable::write_bytes` writes at the current
    // position, so an append channel taking this path would land its bytes
    // wherever the cursor happened to be — the silent
    // wrong-bytes-on-every-subsequent-operation failure the parent page named
    // as the reason not to attempt a fast path at all.
    //
    // Absent field: refuse. Assuming `false` is the same bug with an extra
    // step.
    if !for_read {
        match ctx.get_field_by_name(fd_obj, "append") {
            Value::Int(0) => {}
            _ => return None,
        }
    }
    let fd = fd_from_descriptor(ctx, fd_obj)?;
    let position_lock = ref_field(ctx, this, cf.position_lock)?;
    let interruptor = ctx.get_field(this, cf.interruptor);

    let pos = int_field(ctx, dst, bf.position);
    let lim = int_field(ctx, dst, bf.limit);
    let offset = int_field(ctx, dst, bf.offset);
    if pos < 0 || lim < pos || offset < 0 {
        return None;
    }
    let rem = (lim - pos) as usize;
    let base = (offset as usize).checked_add(pos as usize)?;
    if base.checked_add(rem)? > ctx.array_length(hb) {
        return None;
    }

    Some(FastState {
        cf,
        bf,
        tf,
        thread,
        position_lock,
        interruptor,
        fd,
        hb,
        base,
        pos,
        rem,
    })
}

/// Reproduce `AbstractInterruptibleChannel.end(completed)`'s exceptional tail,
/// but only when it can actually throw.
///
/// The common case — no pending interrupt, channel still open — returns
/// without entering a Java frame at all. The rare case delegates to the JDK's
/// own `end`, so `ClosedByInterruptException` and `AsynchronousCloseException`
/// carry exactly the JDK's semantics (including `interruptor.postInterrupt()`
/// and the dummy-object swap that avoids retaining the thread).
fn finish(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    cf: ChannelFields,
    completed: bool,
) -> Result<(), MethodCallFailed> {
    let pending = ref_field(ctx, this, cf.interrupted_target).is_some();
    let closed_now = bool_field(ctx, this, cf.closed);
    if !pending && (completed || !closed_now) {
        return Ok(());
    }
    ctx.invoke_special_bytecode_only(
        "java/nio/channels/spi/AbstractInterruptibleChannel",
        "end",
        "(Z)V",
        &[
            Value::Object(Some(this)),
            Value::Int(if completed { 1 } else { 0 }),
        ],
    )
    .map(|_| ())
}

/// Hand the call back to the channel's own bytecode.
///
/// The target is the SAME method this native is registered for, and that is
/// not a recursion: `invoke_special_bytecode_only` is the "just run this
/// bytecode, no native check" primitive (`vm_exec::invoke_special_bytecode_only_shared`
/// calls `interpreter::execute` directly), which is exactly what it exists
/// for. Refusing to `implRead` instead would be subtly wrong — it would skip
/// `read`'s own `if (jfrTracing && FileReadEvent.enabled()) return
/// traceImplRead(dst);` branch, so a JFR recording would silently lose the
/// event this path declines to emit.
fn refuse(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    dst: Value,
    method: &str,
) -> MethodCallResult {
    ctx.invoke_special_bytecode_only(
        FCI,
        method,
        "(Ljava/nio/ByteBuffer;)I",
        &[Value::Object(Some(this)), dst],
    )
}

fn native_fc_read_bytebuffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = obj_at(args, 0) else {
        return Err(io_error("FileChannelImpl.read: null receiver"));
    };
    let dst_value = args.get(1).copied().unwrap_or(Value::Object(None));
    let Some(dst) = obj_at(args, 1) else {
        stats::bump(&stats::READ_REFUSED);
        return refuse(ctx, this, dst_value, "read");
    };
    let Some(st) = screen(ctx, this, dst, true) else {
        stats::bump(&stats::READ_REFUSED);
        return refuse(ctx, this, dst_value, "read");
    };
    stats::bump(&stats::READ_FAST);

    ctx.monitor_enter(st.position_lock);
    blocked_on(ctx, st.thread, st.tf, st.interruptor);

    // `implRead`'s own `if (!isOpen()) return 0;` under the position lock: a
    // concurrent close between the screen above and here.
    if bool_field(ctx, this, st.cf.closed) {
        blocked_on(ctx, st.thread, st.tf, Value::Object(None));
        ctx.monitor_exit(st.position_lock);
        finish(ctx, this, st.cf, false)?;
        return Ok(Some(Value::Int(0)));
    }

    let outcome = if st.rem == 0 {
        // `IOUtil.readIntoNativeBuffer` answers 0 for an empty destination
        // before touching the fd, and `IOStatus.normalize(0)` is 0.
        Ok(0usize)
    } else {
        let mut buf = vec![0u8; st.rem];
        // The `fd_table()` borrow is immutable and the heap writes below are
        // mutable, so the read has to complete and yield an OWNED result
        // before the copy-out begins.
        let read = ctx.fd_table().read_bytes(st.fd, &mut buf);
        match read {
            Ok(n) => {
                if n > 0 {
                    ctx.write_byte_array_from(st.hb, st.base, &buf[..n]);
                    ctx.set_field(dst, st.bf.position, Value::Int(st.pos + n as i32));
                }
                Ok(n)
            }
            Err(e) => Err(io_error(format!("FileChannelImpl.read: {e}"))),
        }
    };

    blocked_on(ctx, st.thread, st.tf, Value::Object(None));
    ctx.monitor_exit(st.position_lock);

    let n = match outcome {
        Ok(n) => n,
        Err(e) => {
            finish(ctx, this, st.cf, false)?;
            return Err(e);
        }
    };
    finish(ctx, this, st.cf, n > 0)?;

    // EOF is `IOStatus.EOF` (-1) through `IOStatus.normalize`. A zero-length
    // destination is 0 and is NOT EOF — which is why `rem == 0` short-circuits
    // above instead of falling into a read that would report 0 bytes.
    if st.rem != 0 && n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    Ok(Some(Value::Int(n as i32)))
}

fn native_fc_write_bytebuffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = obj_at(args, 0) else {
        return Err(io_error("FileChannelImpl.write: null receiver"));
    };
    let src_value = args.get(1).copied().unwrap_or(Value::Object(None));
    let Some(src) = obj_at(args, 1) else {
        stats::bump(&stats::WRITE_REFUSED);
        return refuse(ctx, this, src_value, "write");
    };
    let Some(st) = screen(ctx, this, src, false) else {
        stats::bump(&stats::WRITE_REFUSED);
        return refuse(ctx, this, src_value, "write");
    };
    stats::bump(&stats::WRITE_FAST);

    ctx.monitor_enter(st.position_lock);
    blocked_on(ctx, st.thread, st.tf, st.interruptor);

    if bool_field(ctx, this, st.cf.closed) {
        blocked_on(ctx, st.thread, st.tf, Value::Object(None));
        ctx.monitor_exit(st.position_lock);
        finish(ctx, this, st.cf, false)?;
        return Ok(Some(Value::Int(0)));
    }

    let outcome = if st.rem == 0 {
        Ok(0usize)
    } else {
        let mut buf = vec![0u8; st.rem];
        let got = ctx.read_byte_array_into(st.hb, st.base, &mut buf);
        if got != st.rem {
            Err(io_error("FileChannelImpl.write: short read of backing array"))
        } else {
            // Same borrow shape as the read path: finish with the table
            // before touching the heap.
            let written = ctx.fd_table().write_bytes(st.fd, &buf);
            match written {
                Ok(()) => {
                    ctx.set_field(src, st.bf.position, Value::Int(st.pos + st.rem as i32));
                    Ok(st.rem)
                }
                Err(e) => Err(io_error(format!("FileChannelImpl.write: {e}"))),
            }
        }
    };

    blocked_on(ctx, st.thread, st.tf, Value::Object(None));
    ctx.monitor_exit(st.position_lock);

    let n = match outcome {
        Ok(n) => n,
        Err(e) => {
            finish(ctx, this, st.cf, false)?;
            return Err(e);
        }
    };
    finish(ctx, this, st.cf, n > 0)?;
    Ok(Some(Value::Int(n as i32)))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_file_channel_fast_io(r: &mut NativeMethodRegistry) {
    if !engaged() {
        return;
    }
    let prev = r.current_category();
    r.set_category(NativeKind::Intrinsic);
    r.register_with_kind(
        FCI,
        "read",
        "(Ljava/nio/ByteBuffer;)I",
        native_fc_read_bytebuffer,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        FCI,
        "write",
        "(Ljava/nio/ByteBuffer;)I",
        native_fc_write_bytebuffer,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(FCI, "position", "()J", native_fc_position, NativeKind::Intrinsic);
    // The `SeekableByteChannel`-returning overload is javac's bridge and calls
    // this one, so only the declared shape is registered.
    r.register_with_kind(
        FCI,
        "position",
        "(J)Ljava/nio/channels/FileChannel;",
        native_fc_position_set,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(FCI, "size", "()J", native_fc_size, NativeKind::Intrinsic);
    r.set_category(prev);
}
