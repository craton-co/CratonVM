use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

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

// --- W7-53 (2026-08-12): close-awareness ---

/// The row this whole family is about: a thread parked in a blocking pipe
/// read must come back when another thread closes the end.
///
/// RED-by-construction against the pre-2026-08-12 tree: `read_pipe` there
/// parked in `ReadFile`/`read(2)` with `lpOverlapped = NULL`, and
/// `close_pipe_end` neither woke it nor could have — it closed the handle
/// out from under it instead.
///
/// The read is proved to be genuinely parked before the close, not merely
/// racing it: the reader publishes `started` and the closing thread waits
/// for that AND for a further 100 ms, which is 20 poll slices, before it
/// closes anything. A close issued ahead of the read would prove nothing,
/// because the read would then return for an unrelated reason.
#[test]
fn a_close_wakes_a_reader_parked_on_a_pipe() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
    let r_id = register_pipe_end(read_end);
    let started = Arc::new(AtomicBool::new(false));
    let reader_started = Arc::clone(&started);
    let reader = std::thread::spawn(move || {
        let end = pipe_enter(r_id).expect("open at entry");
        reader_started.store(true, Ordering::SeqCst);
        let mut buf = [0u8; 16];
        let result = pipe_read_close_aware(r_id, end.raw, &mut buf);
        pipe_leave(r_id);
        result.map(|n| n as i64).map_err(|e| e.kind())
    });
    while !started.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    // Nobody writes to `write_end`, so the reader is parked on an empty
    // pipe for the whole of this sleep.
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!reader.is_finished(), "the read did not park at all");
    close_pipe_end(r_id);
    let outcome = reader.join().expect("reader thread");
    assert_eq!(
        outcome,
        Err(std::io::ErrorKind::Interrupted),
        "a parked pipe read must observe the close"
    );
    close_raw(write_end.raw);
}

/// The close-aware read is still a read: the wakeup must not cost payload.
#[test]
fn a_close_aware_pipe_read_still_delivers_bytes() {
    let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
    let r_id = register_pipe_end(read_end);
    write_pipe(write_end.raw, b"payload").expect("write");
    let end = pipe_enter(r_id).expect("open");
    let mut buf = [0u8; 16];
    let n = pipe_read_close_aware(r_id, end.raw, &mut buf).expect("read");
    pipe_leave(r_id);
    assert_eq!(&buf[..n as usize], b"payload");
    close_pipe_end(r_id);
    close_raw(write_end.raw);
}

/// The other half of the mechanism: a close arriving while an operation is
/// in flight must NOT close the raw handle underneath it.
///
/// Before 2026-08-12 `close_pipe_end` called `close_raw` unconditionally,
/// so the handle a parked thread was mid-syscall on was freed and its
/// number could be recycled onto an unrelated file. That is why the
/// generation check alone is not the fix here and is the fix on every
/// socket site in this family: those park holding an `Arc<TcpStream>`,
/// which keeps the handle alive for them.
#[test]
fn a_close_during_an_in_flight_op_defers_the_raw_close() {
    let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
    let r_id = register_pipe_end(read_end);
    let entered = pipe_enter(r_id).expect("open");
    assert!(close_pipe_end(r_id), "first close reports it did the work");
    let mid = pipe_end_get(r_id).expect("entry survives the close");
    assert!(mid.closed, "the generation flipped immediately");
    assert!(
        mid.close_pending,
        "the raw close is deferred while an op is in flight"
    );
    assert!(
        !pipe_still_open(r_id),
        "a parked op re-asking now must see the close"
    );
    // The handle is still valid here, which is the whole point: a thread
    // mid-syscall on `entered.raw` has not had it freed under it.
    assert_eq!(entered.raw, mid.raw);
    pipe_leave(r_id);
    let after = pipe_end_get(r_id).expect("entry still present");
    assert!(
        !after.close_pending,
        "the last op out performs the deferred close"
    );
    close_raw(write_end.raw);
}

// --- AUDIT 2026-07-26 (native-io-audit) regressions ---

use crate::test_support::MockNativeContext;

/// Build a real-JDK-shaped `HeapByteBuffer` over a fresh byte[] of
/// `cap` bytes, with the given `offset`/`position`/`limit`.
fn heap_buffer(
    ctx: &mut MockNativeContext,
    cap: usize,
    offset: i32,
    position: i32,
    limit: i32,
) -> (ObjectRef, ObjectRef) {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, cap);
    let bb = ctx.alloc_object(8);
    ctx.set_field_by_name(bb, "hb", Value::Object(Some(arr)));
    ctx.set_field_by_name(bb, "offset", Value::Int(offset));
    ctx.set_field_by_name(bb, "position", Value::Int(position));
    ctx.set_field_by_name(bb, "limit", Value::Int(limit));
    (bb, arr)
}

/// `buffer_view` used to drop the ByteBuffer `offset` field entirely and
/// index the backing array by the raw `position`. Every buffer produced
/// by `slice()` carries a non-zero `offset` (HeapByteBuffer.slice() resets
/// position to 0 and folds the parent position into offset), so a pipe
/// read/write against a sliced buffer silently touched the WRONG region
/// of the backing array — no exception, wrong bytes.
#[test]
fn audit_buffer_view_honours_slice_offset() {
    let mut ctx = MockNativeContext::new();
    // A slice of a 16-byte array starting at index 6, 4 bytes long:
    // offset=6, position=0, limit=4.
    let (bb, _arr) = heap_buffer(&mut ctx, 16, 6, 0, 4);
    let view = buffer_view(&ctx, bb).expect("heap layout recognised");
    assert_eq!(view.base, 6, "offset must be read");
    assert_eq!(view.index(), 6, "array index is offset + position");
    assert_eq!(view.remaining(), 4);

    // With a non-zero position too: offset=6, position=1 -> index 7.
    ctx.set_field_by_name(bb, "position", Value::Int(1));
    let view = buffer_view(&ctx, bb).expect("heap layout recognised");
    assert_eq!(view.index(), 7);
    assert_eq!(view.remaining(), 3);
}

/// A plain `ByteBuffer.allocate()` (offset absent / 0) must be unchanged
/// by the fix.
#[test]
fn audit_buffer_view_plain_allocate_unchanged() {
    let mut ctx = MockNativeContext::new();
    let (bb, _arr) = heap_buffer(&mut ctx, 8, 0, 2, 6);
    let view = buffer_view(&ctx, bb).expect("heap layout recognised");
    assert_eq!(view.base, 0);
    assert_eq!(view.index(), 2);
    assert_eq!(view.remaining(), 4);
}

/// The synthetic layout has no `offset` field at all; a missing (or
/// non-Int) read must be treated as 0, not as a bogus base.
#[test]
fn audit_buffer_view_missing_offset_field_is_zero() {
    let mut ctx = MockNativeContext::new();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    let bb = ctx.alloc_object(8);
    ctx.set_field_by_name(bb, "hb", Value::Object(Some(arr)));
    ctx.set_field_by_name(bb, "position", Value::Int(1));
    ctx.set_field_by_name(bb, "limit", Value::Int(3));
    // No "offset" named field set -> get_field_by_name yields Object(None).
    let view = buffer_view(&ctx, bb).expect("heap layout recognised");
    assert_eq!(view.base, 0);
    assert_eq!(view.index(), 1);
}

/// A sliced destination buffer must receive the pipe bytes at
/// `offset + position`, and the blocking read must be bracketed in a
/// GC-cooperative blocking region.
#[test]
fn audit_source_read_writes_at_slice_offset_and_brackets_gc() {
    let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
    let read_id = register_pipe_end(read_end);
    let payload = b"XYZ";
    assert_eq!(
        write_pipe(write_end.raw, payload).expect("write") as usize,
        payload.len()
    );
    close_raw(write_end.raw);

    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(4);
    ctx.set_field(this, PIPE_FIELD_ID, Value::Int(read_id));
    // 16-byte array, slice starting at 6 with room for 3 bytes.
    let (bb, arr) = heap_buffer(&mut ctx, 16, 6, 0, 3);

    let before = ctx.blocking_region_counts();
    let r = source_read_buffer(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(bb))],
    );
    assert!(matches!(r, Ok(Some(Value::Int(3)))), "read 3 bytes");

    // Bytes must land at arr[6..9], NOT arr[0..3].
    let mut got = vec![0u8; 16];
    ctx.read_byte_array_into(arr, 0, &mut got);
    assert_eq!(
        &got[6..9],
        &payload[..],
        "pipe bytes must be written at offset + position"
    );
    assert_eq!(
        &got[0..3],
        &[0u8, 0, 0][..],
        "nothing may be written at the raw position"
    );
    // position is buffer-relative, so it advances 0 -> 3.
    assert_eq!(ctx.get_field_by_name(bb, "position"), Value::Int(3));

    let after = ctx.blocking_region_counts();
    assert_eq!(
        (after.0 - before.0, after.1 - before.1),
        (1, 1),
        "the blocking kernel read must be bracketed exactly once"
    );
    close_pipe_end(read_id);
}

/// The sink write must read from `offset + position` too, and bracket the
/// (potentially blocking) kernel write.
#[test]
fn audit_sink_write_reads_from_slice_offset_and_brackets_gc() {
    let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
    let write_id = register_pipe_end(write_end);

    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(4);
    ctx.set_field(this, PIPE_FIELD_ID, Value::Int(write_id));
    let (bb, arr) = heap_buffer(&mut ctx, 16, 6, 0, 3);
    // Put a decoy at the raw position and the real payload at the slice.
    ctx.write_byte_array_from(arr, 0, b"BAD");
    ctx.write_byte_array_from(arr, 6, b"OK!");

    let before = ctx.blocking_region_counts();
    let r = sink_write_buffer(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(bb))],
    );
    assert!(matches!(r, Ok(Some(Value::Int(3)))));
    let after = ctx.blocking_region_counts();
    assert_eq!((after.0 - before.0, after.1 - before.1), (1, 1));

    let mut got = [0u8; 3];
    let n = read_pipe(read_end.raw, &mut got).expect("read back");
    assert_eq!(n, 3);
    assert_eq!(
        &got[..],
        &b"OK!"[..],
        "the bytes on the wire must come from offset + position"
    );
    assert_eq!(ctx.get_field_by_name(bb, "position"), Value::Int(3));
    close_pipe_end(write_id);
    close_raw(read_end.raw);
}

/// A `limit` that overshoots the backing array must clamp rather than
/// read/write out of range.
#[test]
fn audit_buffer_view_clamps_limit_past_array_end() {
    let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
    let read_id = register_pipe_end(read_end);
    write_pipe(write_end.raw, b"abcdefgh").expect("write");
    close_raw(write_end.raw);

    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(4);
    ctx.set_field(this, PIPE_FIELD_ID, Value::Int(read_id));
    // 4-byte array, offset 2 -> only 2 bytes addressable, but limit says 8.
    let (bb, arr) = heap_buffer(&mut ctx, 4, 2, 0, 8);
    let r = source_read_buffer(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(bb))],
    );
    assert!(
        matches!(r, Ok(Some(Value::Int(n))) if n <= 2),
        "must not claim more bytes than the array can hold, got {r:?}"
    );
    let mut got = vec![0u8; 4];
    ctx.read_byte_array_into(arr, 0, &mut got);
    assert_eq!(&got[2..4], &b"ab"[..]);
    close_pipe_end(read_id);
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

// -----------------------------------------------------------------------
// G46-1 (2026-08-17) — the private slot map, and the two real fields it
// used to alias.
//
// MEASURED BEFORE, `target-rel3` (`9ae371468`), `--real-jdk`,
// `CRATONVM_DBG_COERCION=1`, one `Pipe.open()`: six
// `primitive-into-reference` events with `descriptor=L` at the three
// `alloc_channel` lines, and `G46PipeProbe` died at the first write with
// `java.io.IOException: SinkChannel.write: missing pipe id`.
// -----------------------------------------------------------------------

/// The real JDK 25 transitive instance layout of
/// `sun.nio.ch.SourceChannelImpl` / `SinkChannelImpl`, from `javap -p`
/// against Adoptium 25.0.3+9 — superclass first, statics excluded.
///
/// This is the table the pre-G46 slot map collided with, written down so
/// the collision is a property the test can state rather than a fact in a
/// comment.
const REAL_CHANNEL_LAYOUT: [(&str, &str); 11] = [
    ("closeLock", "Ljava/lang/Object;"),
    ("closed", "Z"),
    ("interruptor", "Lsun/nio/ch/Interruptible;"),
    ("interruptedTarget", "Ljava/lang/Object;"),
    ("provider", "Ljava/nio/channels/spi/SelectorProvider;"),
    ("keys", "[Ljava/nio/channels/SelectionKey;"),
    ("keyCount", "I"),
    ("keyLock", "Ljava/lang/Object;"),
    ("regLock", "Ljava/lang/Object;"),
    ("nonBlocking", "Z"),
    ("sc", "Ljava/nio/channels/SocketChannel;"),
];

/// The arithmetic half: on the real class the private map starts above
/// every declared field, so no private slot can land on a reference the
/// class declares — which is the entire defect.
///
/// RED against the pre-G46 tree, where the map was the absolute 0..3 this
/// test's first assertion now forbids.
#[test]
fn the_private_slot_map_clears_every_declared_reference_slot() {
    let base = REAL_CHANNEL_LAYOUT.len();
    let private = [
        PIPE_FIELD_ID,
        PIPE_FIELD_OPEN,
        PIPE_FIELD_KIND,
        PIPE_FIELD_BLOCKING,
    ];
    assert_eq!(
        private.len(),
        PIPE_CHANNEL_PRIVATE_SLOTS,
        "the width must cover every private slot the allocator writes"
    );
    for p in private {
        let absolute = base + p;
        assert!(
            REAL_CHANNEL_LAYOUT.get(absolute).is_none(),
            "private slot {p} lands on {:?}, which the class declares",
            REAL_CHANNEL_LAYOUT.get(absolute)
        );
    }
    // And the three that the coercion actually destroyed are reference
    // slots — the reason a primitive there became null rather than merely
    // being in the wrong place.
    for slot in [PIPE_FIELD_ID, PIPE_FIELD_KIND, PIPE_FIELD_BLOCKING] {
        let (name, desc) = REAL_CHANNEL_LAYOUT[slot];
        assert!(
            desc.starts_with('L') || desc.starts_with('['),
            "{name} was expected to be a reference slot, got {desc}"
        );
    }
    // …and the fourth aliased `closed`, which is the same WIDTH but the
    // opposite MEANING: `isOpen()` is `!closed`.
    assert_eq!(REAL_CHANNEL_LAYOUT[PIPE_FIELD_OPEN], ("closed", "Z"));
}

/// A fresh channel must read back `closed = false` on the field the class
/// declares, not `true`.
///
/// RED against the pre-G46 tree: nothing wrote `closed` by name there, and
/// the indexed open flag put `Int(1)` on that very slot — so a brand-new
/// pipe channel told real `AbstractInterruptibleChannel.isOpen()` bytecode
/// that it was already closed.
#[test]
fn a_fresh_channel_is_not_closed_on_the_field_the_class_declares() {
    let mut ctx = MockNativeContext::new();
    let obj = alloc_channel(&mut ctx, "sun/nio/ch/SourceChannelImpl", false, 41);
    assert_eq!(
        ctx.get_field_by_name(obj, "closed"),
        Value::Int(0),
        "a fresh channel is open, i.e. closed == false"
    );
    let base = channel_private_base(&mut ctx, obj);
    assert_eq!(ctx.get_field(obj, base + PIPE_FIELD_ID), Value::Int(41));
    assert_eq!(ctx.get_field(obj, base + PIPE_FIELD_OPEN), Value::Int(1));
    assert_eq!(ctx.get_field(obj, base + PIPE_FIELD_KIND), Value::Int(0));
    assert_eq!(
        ctx.get_field(obj, base + PIPE_FIELD_BLOCKING),
        Value::Int(1)
    );
}

/// The other end of the same inversion, and the `ServerSocket.bound` shape
/// this record is about: `close()` must set `closed = true`.
///
/// RED against the pre-G46 tree, where `channel_close` wrote `Int(0)` at
/// the slot the real class declares as `closed` — so closing the channel
/// told the JDK it had just become OPEN.
#[test]
fn closing_a_channel_sets_the_real_closed_field_true() {
    let (read_end, write_end) = create_anonymous_pipe().expect("pipe");
    let read_id = register_pipe_end(read_end);
    let mut ctx = MockNativeContext::new();
    let obj = alloc_channel(&mut ctx, "sun/nio/ch/SourceChannelImpl", false, read_id);
    assert_eq!(
        channel_is_open(&mut ctx, &[Value::Object(Some(obj))]).unwrap(),
        Some(Value::Int(1))
    );
    channel_close(&mut ctx, &[Value::Object(Some(obj))]).expect("close");
    assert_eq!(
        ctx.get_field_by_name(obj, "closed"),
        Value::Int(1),
        "close() must set closed = true, not false"
    );
    assert_eq!(
        channel_is_open(&mut ctx, &[Value::Object(Some(obj))]).unwrap(),
        Some(Value::Int(0)),
        "isOpen() must answer false after close()"
    );
    close_raw(write_end.raw);
}

/// `AbstractSelectableChannel.isBlocking()` is `return !nonBlocking`, so
/// the real field is the NEGATION of `configureBlocking`'s argument.
///
/// RED against the pre-G46 tree, which wrote the argument verbatim at
/// absolute slot 3 — `interruptedTarget : Object` — where the coercion
/// nulled it, and never touched `nonBlocking` at all.
#[test]
fn configure_blocking_records_the_negation_on_the_real_field() {
    let mut ctx = MockNativeContext::new();
    let obj = alloc_channel(&mut ctx, "sun/nio/ch/SinkChannelImpl", true, 5);
    channel_configure_blocking(&mut ctx, &[Value::Object(Some(obj)), Value::Int(0)])
        .expect("configureBlocking(false)");
    assert_eq!(ctx.get_field_by_name(obj, "nonBlocking"), Value::Int(1));
    let base = channel_private_base(&mut ctx, obj);
    assert_eq!(
        ctx.get_field(obj, base + PIPE_FIELD_BLOCKING),
        Value::Int(0)
    );

    channel_configure_blocking(&mut ctx, &[Value::Object(Some(obj)), Value::Int(1)])
        .expect("configureBlocking(true)");
    assert_eq!(ctx.get_field_by_name(obj, "nonBlocking"), Value::Int(0));
    assert_eq!(
        ctx.get_field(obj, base + PIPE_FIELD_BLOCKING),
        Value::Int(1)
    );
}

/// The width guard. A receiver this crate did not allocate — anything too
/// narrow for `base + PIPE_CHANNEL_PRIVATE_SLOTS` — must fall back to the
/// legacy base rather than index past the object.
///
/// This is what keeps the fix from turning a foreign receiver into an
/// out-of-range access, and it is also what reconciles the
/// `AnonymousObject$4` substitution the allocator's failure arm produces.
#[test]
fn a_receiver_too_narrow_for_the_private_map_uses_the_legacy_base() {
    let mut ctx = MockNativeContext::new();
    let narrow = ctx.alloc_object_with_class(1, "sun/nio/ch/SourceChannelImpl");
    assert_eq!(channel_private_base(&mut ctx, narrow), 0);
    // And every accessor must still answer rather than panic or refuse in
    // a new way: no pipe id at slot 0 is the pre-existing IOException.
    let r = source_read_buffer(&mut ctx, &[Value::Object(Some(narrow))]);
    assert!(
        r.is_err(),
        "a receiver with no pipe id still raises IOException"
    );
}

/// Source tripwire: every private access must go through the resolved
/// base. Scans only what is ABOVE this test module, because the needles
/// appear verbatim in the assertions here.
#[test]
fn every_private_pipe_slot_access_is_base_relative() {
    let src = include_str!("../pipe.rs")
        .split("mod tests {")
        .next()
        .expect("split always yields a first element");
    for needle in [
        "ctx.get_field(this, PIPE_FIELD_ID)",
        "ctx.set_field(obj, PIPE_FIELD_ID",
        "ctx.get_field(this, PIPE_FIELD_OPEN)",
        "ctx.set_field(this, PIPE_FIELD_OPEN",
    ] {
        assert!(
            !src.contains(needle),
            "`{needle}` indexes the private map from 0, which on the real \
             sun.nio.ch.*ChannelImpl layout is closeLock/closed (G46-1)"
        );
    }
    assert!(
        src.contains("let base = channel_private_base(ctx, this);"),
        "the accessors must resolve the private base from the receiver"
    );
}
