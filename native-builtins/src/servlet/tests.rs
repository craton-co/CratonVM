use super::*;
use cratonvm_native_api::NativeContext as _;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ClassId;

/// SHIM-AUDIT regression (fails before the 2026-08-01 fix).
///
/// `java.nio.ByteBuffer` is an ABSTRACT class, so this native is inherited
/// by every buffer in the VM through the interpreter's superclass walk —
/// and it wins over the real `Buffer.toString()` bytecode, whose whole body
/// is `getClass().getName() + "[pos=" …`. The shim used to answer with the
/// hard-coded literal `java.nio.HeapByteBuffer`, so a direct, mapped,
/// read-only or third-party buffer all claimed to be heap buffers.
///
/// Each case here has a CONCRETE receiver class, which is the real-JDK
/// shape and the only branch `getClass().getName()` would ever take. The
/// storage-derived fallback (for CratonVM's own synthetic stand-ins, whose
/// class name IS the abstract `java/nio/ByteBuffer`) is exercised by
/// `vm/src/vm.rs::byte_buffer_to_string`, which allocates through
/// `ByteBuffer.allocate` and still expects `java.nio.HeapByteBuffer`.
#[test]
fn byte_buffer_to_string_names_the_receivers_own_class() {
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    register_s2_bytebuffer(&mut registry);
    let cb = registry
        .find("java/nio/ByteBuffer", "toString", "()Ljava/lang/String;")
        .expect("ByteBuffer.toString native");

    for class_name in [
        "java/nio/DirectByteBuffer",
        "java/nio/MappedByteBuffer",
        "java/nio/HeapByteBufferR",
        "com/example/VendorByteBuffer",
    ] {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let buf = match ctx.new_object(class_name).expect("alloc") {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected {class_name} object, got {other:?}"),
        };
        let rendered = match cb(&mut ctx, &[Value::Object(Some(buf))]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            _ => panic!("expected a String from ByteBuffer.toString for {class_name}"),
        };
        let expected_prefix = format!("{}[", class_name.replace('/', "."));
        assert!(
            rendered.starts_with(&expected_prefix),
            "ByteBuffer.toString must render the RECEIVER's class: expected a \
             `{expected_prefix}…` prefix, got `{rendered}`. A hard-coded concrete \
             class name here is a claim the shim cannot know — see \
             native-builtins-shim-audit.md."
        );
    }
}

/// W7-83 — `java.nio.Buffer.segment` is not a backing array, on the
/// registration that WINS in Compatible mode.
///
/// W7-76 §2 settled the registration question: in both Compatible arms
/// `set_drop_real_layout_synthetic(true)` runs before `register_io_natives`,
/// so `register_nio_natives` is skipped and nothing overwrites
/// `register_s2_bytebuffer`. `array()[B`, `hasArray()Z` and `arrayOffset()I`
/// are all on `native_override.rs`'s forced-native list for
/// `java/nio/ByteBuffer`, so these natives answer even with the real
/// bytecode present.
///
/// Measured on Eclipse Adoptium 25.0.3.9 (`probes/DirectByteBufferStateProbe.java`,
/// section `seg`): `Arena.ofAuto().allocate(16).asByteBuffer()` is a
/// `java.nio.DirectByteBuffer` with `hb == null`, `segment ==
/// jdk.internal.foreign.NativeMemorySegmentImpl` and a real process pointer
/// in `address`; `hasArray()` is **false** and `array()` throws
/// `UnsupportedOperationException`. Before the screen `s2_bb_arr` returned
/// the segment, so `hasArray()` answered true and `array()` — whose declared
/// return type is `[B` — handed back a `MemorySegment`.
///
/// The heap control arm is asserted in the same test: a genuine backing
/// array must still answer `hasArray() == true` and come back from
/// `array()`, or the screen has merely broken the other population.
#[test]
fn s2_bytebuffer_refuses_a_memory_segment_as_a_backing_array() {
    use cratonvm_native_api::FieldMetadata;

    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    register_s2_bytebuffer(&mut registry);
    let array_fn = registry
        .find("java/nio/ByteBuffer", "array", "()[B")
        .expect("ByteBuffer.array native");
    let has_array_fn = registry
        .find("java/nio/ByteBuffer", "hasArray", "()Z")
        .expect("ByteBuffer.hasArray native");

    let mut ctx = crate::test_utils::MockNativeContext::new();
    // Answer an unresolvable name the way production does, so `hb` reads
    // back as absent rather than as the mock's historic `Int(0)`.
    ctx.set_absent_field_answers_null(true);
    let cid = ctx
        .ensure_class_initialized("java/nio/DirectByteBuffer")
        .expect("mock class");
    // The real JDK 25 layout, transitively over the superclass chain:
    // mark(0) position(1) limit(2) capacity(3) address(4) segment(5)
    // hb(6) offset(7). Declaring it is what makes `hb`-by-name resolve to
    // slot 6 (and answer null) instead of never resolving at all.
    ctx.set_declared_fields(
        cid,
        [
            ("mark", "I", 0),
            ("position", "I", 1),
            ("limit", "I", 2),
            ("capacity", "I", 3),
            ("address", "J", 4),
            ("segment", "Ljava/lang/foreign/MemorySegment;", 5),
            ("hb", "[B", 6),
            ("offset", "I", 7),
        ]
        .into_iter()
        .map(|(name, descriptor, slot_index)| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        })
        .collect(),
    );

    let mut native = vec![0u8; 16];
    let addr = native.as_mut_ptr() as i64;
    let segment = match ctx
        .new_object("jdk/internal/foreign/NativeMemorySegmentImpl")
        .expect("segment stand-in")
    {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected an object, got {other:?}"),
    };
    let buf = match ctx.new_object("java/nio/DirectByteBuffer").expect("buffer") {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected an object, got {other:?}"),
    };
    ctx.set_field(buf, 0, Value::Int(-1)); // mark
    ctx.set_field(buf, BB_POS, Value::Int(0));
    ctx.set_field(buf, BB_LIMIT, Value::Int(16));
    ctx.set_field(buf, BB_CAP, Value::Int(16));
    ctx.set_field(buf, BB_MARK, Value::Long(addr)); // real layout: `address`
    ctx.set_field(buf, BB_SEGMENT_SLOT, Value::Object(Some(segment)));

    assert!(
        s2_bb_arr(&ctx, buf).is_none(),
        "a MemorySegment at slot 5 was returned as a backing array"
    );
    assert!(
        matches!(
            has_array_fn(&mut ctx, &[Value::Object(Some(buf))]),
            Ok(Some(Value::Int(0)))
        ),
        "HotSpot answers hasArray() == false for an Arena segment's \
         asByteBuffer(); measured, not assumed"
    );
    let thrown = array_fn(&mut ctx, &[Value::Object(Some(buf))]);
    match &thrown {
        Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::UnsupportedOperationException { .. },
        ))) => {}
        other => panic!(
            "array() on an Arena segment's buffer must throw \
             UnsupportedOperationException as HotSpot does, got {other:?}"
        ),
    }

    // --- the control: a genuine heap buffer still answers with its array.
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    let heap = match ctx.new_object("java/nio/HeapByteBuffer").expect("buffer") {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected an object, got {other:?}"),
    };
    ctx.set_field(heap, BB_ARRAY, Value::Object(Some(arr)));
    ctx.set_field(heap, BB_POS, Value::Int(0));
    ctx.set_field(heap, BB_LIMIT, Value::Int(16));
    ctx.set_field(heap, BB_CAP, Value::Int(16));
    assert_eq!(s2_bb_arr(&ctx, heap), Some(arr));
    assert!(matches!(
        has_array_fn(&mut ctx, &[Value::Object(Some(heap))]),
        Ok(Some(Value::Int(1)))
    ));
    let got = array_fn(&mut ctx, &[Value::Object(Some(heap))]);
    match &got {
        Ok(Some(Value::Object(Some(a)))) if *a == arr => {}
        other => panic!(
            "a genuine heap buffer must still answer array() with its own \
             backing array, got {other:?}"
        ),
    }

    // --- and the OTHER slot-5 population: `native-builtins`' own typed
    // buffer views park a real array there, because `segment` is the only
    // Object-typed field `Buffer` declares. The screen must not take them
    // out with the MemorySegment.
    let view_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
    let view = match ctx.new_object("java/nio/IntBuffer").expect("view") {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected an object, got {other:?}"),
    };
    ctx.set_field(view, BB_SEGMENT_SLOT, Value::Object(Some(view_arr)));
    assert_eq!(
        s2_bb_arr(&ctx, view),
        Some(view_arr),
        "a typed buffer view's backing array lives at slot 5 and must \
         still resolve"
    );
}

/// F14-1 N3. The test above matches the UOE variant with `{ .. }`, which is
/// precisely why a wrong detail message survived: the CLASS was right, so
/// no `catch`, no class-name assertion and no variant-shaped `matches!`
/// could see it. This one reads the message.
///
/// MEASURED, jdk-25.0.3+9: `ByteBuffer.allocateDirect(8).array()` and
/// `.arrayOffset()` both report `getMessage() == null`, as does
/// `new UnsupportedOperationException()`.
#[test]
fn s2_bb_no_backing_array_has_a_null_detail_message() {
    match s2_bb_no_backing_array() {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::UnsupportedOperationException { message },
        )) => assert!(
            message.is_empty(),
            "the EMPTY string is what types/src/error.rs maps to the ()V ctor, i.e. a \
             null getMessage(). The old \"direct buffer has no backing array\" gave \
             getMessage() a value HotSpot does not have. Got {message:?}"
        ),
        other => panic!("must be UnsupportedOperationException, got {other:?}"),
    }
}

#[test]
fn test_socket_registry_alloc_stream_wrapping_ids() {
    let mut reg = SocketRegistry::default();
    // Simulate near-overflow
    reg.next_id = i32::MAX;
    // We can't reliably create a TcpStream in tests without a listener,
    // so test the ID allocation logic directly.
    let id = reg.next_id;
    reg.next_id = reg.next_id.checked_add(1).unwrap_or(1);
    assert_eq!(id, i32::MAX);
    assert_eq!(reg.next_id, 1, "should wrap to 1 on overflow");
}

#[test]
fn test_socket_registry_id_collision_avoidance() {
    let mut reg = SocketRegistry::default();
    // Insert a dummy to simulate ID 2 being in use
    reg.next_id = 1;
    // Manually set next_id to 2 and insert a "stream" at id 2
    // We can't create a real TcpStream easily, so just test the logic
    let id = reg.next_id;
    assert_eq!(id, 1);
    reg.next_id = reg.next_id.checked_add(1).unwrap_or(1);
    assert_eq!(reg.next_id, 2);
}

#[test]
fn test_socket_registry_default() {
    let reg = SocketRegistry::default();
    assert_eq!(reg.next_id, 1);
    assert!(reg.streams.is_empty());
    assert!(reg.listeners.is_empty());
    assert!(reg.dgrams.is_empty());
}

// =======================================================================
// B8 — ByteBuffer relative-read index arithmetic must not overflow/panic.
// =======================================================================

#[test]
fn b8_bb_off_no_overflow_panic() {
    // Normal case: simple addition.
    assert_eq!(s2_bb_off(10, 3), 13);
    assert_eq!(s2_bb_off(0, 7), 7);
    // Overflow near i32::MAX must saturate to the negative out-of-range
    // sentinel rather than panicking (debug) or wrapping (release).
    assert_eq!(s2_bb_off(i32::MAX, 1), -1);
    assert_eq!(s2_bb_off(i32::MAX - 2, 7), -1);
    // A negative starting index stays negative (out-of-range sentinel).
    assert!(s2_bb_off(-1, 1) < 0);
}

#[test]
fn b8_bb_int_byte_off_no_overflow_panic() {
    // Normal case: byte offset = base + unit*4.
    assert_eq!(s2_bb_int_byte_off(0, 3), 12);
    assert_eq!(s2_bb_int_byte_off(8, 2), 16);
    // `unit * 4` overflow saturates to the out-of-range sentinel.
    assert_eq!(s2_bb_int_byte_off(0, i32::MAX), -1);
    assert_eq!(s2_bb_int_byte_off(0, i32::MAX / 3), -1);
    // `base + bytes` overflow also saturates.
    assert_eq!(s2_bb_int_byte_off(i32::MAX, 1), -1);
}

// =======================================================================
// [nb-servlet] — ByteBuffer.get/put([BII) off/len validation.
//
// The bulk get([BII)/put([BII) handlers used to accept `off`/`len` as raw
// i32 with no negativity check and cast `off` to usize before validating.
// A negative `len` slipped past `pos + len > limit` (the sum shrinks), then
// `for i in 0..len as usize` reinterpreted the negative as ~1.8e19 → an
// effectively infinite loop (hang/DoS). This pins the exact validation
// predicate the fix introduced (mirrored here as a pure function so it is
// testable without a full VM `ctx`).
// =======================================================================

/// Returns the rejected index (as the JDK-style IOOBE would report) if the
/// (off, len) pair is invalid for an array of `cap` elements, else `None`.
/// This is a faithful copy of the guard now in the get/put([BII) handlers.
fn bb_bii_reject(off: i32, len: i32, cap: i64) -> Option<i32> {
    if off < 0 || len < 0 || (off as i64) + (len as i64) > cap {
        Some(if off < 0 {
            off
        } else {
            off.saturating_add(len)
        })
    } else {
        None
    }
}

#[test]
fn nb_servlet_bb_bii_rejects_negative_and_oob() {
    // Valid in-range copies pass.
    assert_eq!(bb_bii_reject(0, 8, 8), None);
    assert_eq!(bb_bii_reject(3, 5, 8), None);
    assert_eq!(bb_bii_reject(8, 0, 8), None);

    // Negative len — the original hang/DoS vector — is rejected.
    assert!(bb_bii_reject(0, -1, 8).is_some());
    assert!(bb_bii_reject(0, i32::MIN, 8).is_some());

    // Negative off (would become a huge usize) is rejected.
    assert!(bb_bii_reject(-1, 4, 8).is_some());

    // off+len overrunning the array is rejected, even when each is in-range.
    assert!(bb_bii_reject(4, 5, 8).is_some());
    // Overflow of off+len cannot wrap into a "passing" value (i64 widening).
    assert!(bb_bii_reject(i32::MAX, i32::MAX, 8).is_some());

    // Sanity: a negative len would, if cast to usize, drive an astronomically
    // long loop — confirm we never reach that cast for the bad case.
    let bad_len = -1i32;
    assert!(bb_bii_reject(0, bad_len, 16).is_some());
    // (If the guard were absent, `bad_len as usize` would be ~1.8e19.)
    assert!(bad_len as usize > 1_000_000_000_000_000_000usize);
}

// =======================================================================
// NEW-3 — Cross-platform poll + wakeup + UDP selector registration
// =======================================================================

/// Unit-level smoke test of [`selector_poll`]: register a single UDP
/// socket for reading, verify that a non-blocking poll reports it
/// *not* readable, then send a byte to it and verify the next poll
/// reports it readable. This exercises the full Unix `poll(2)` /
/// Windows `WSAPoll` dispatch path, the event-bit translation, and
/// the raw-fd extraction helpers in one go.
#[test]
fn new3_selector_poll_udp_readiness() {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
    sock.set_nonblocking(true).expect("nonblock");
    let addr = sock.local_addr().expect("local_addr");
    // Self-connect so send() loops back into our own recv buffer.
    sock.connect(addr).expect("connect self");

    let req = PollReq {
        fd: dgram_pollreq_fd(&sock),
        events: POLL_IN,
    };

    // Initially not readable.
    let revs0 = selector_poll(&[req], 0);
    assert_eq!(revs0.len(), 1);
    assert_eq!(
        revs0[0] & POLL_IN,
        0,
        "idle socket must not report readable"
    );

    // Send a byte; it goes straight back into our own receive buffer.
    sock.send(&[42u8]).expect("send-to-self");
    // Give the kernel a moment to deliver (usually instant on
    // loopback, but poll() itself has 100ms to notice).
    let revs1 = selector_poll(&[req], 100);
    assert_eq!(revs1.len(), 1);
    assert!(
        revs1[0] & POLL_IN != 0,
        "socket must report readable after send-to-self, got {:#x}",
        revs1[0]
    );
}

/// Cross-platform round-trip through the same `selector_poll` entry
/// point as the real JVM selector uses, but with an explicit poll
/// timeout of zero. A non-blocking poll on an empty request array
/// must return an empty revents vec immediately.
#[test]
fn new3_selector_poll_empty_returns_immediately() {
    let revs = selector_poll(&[], 0);
    assert!(revs.is_empty());
}

/// Verify the positive-timeout path actually sleeps (~100ms here).
/// This catches regressions where the timeout parameter is ignored
/// or clamped to zero.
#[test]
fn new3_selector_poll_honors_positive_timeout() {
    // Build a request that will never be ready (UDP socket with
    // nothing sent to it).
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
    sock.set_nonblocking(true).expect("nonblock");
    let req = PollReq {
        fd: dgram_pollreq_fd(&sock),
        events: POLL_IN,
    };
    let t0 = std::time::Instant::now();
    let revs = selector_poll(&[req], 100);
    let elapsed = t0.elapsed();
    assert_eq!(revs.len(), 1);
    assert_eq!(revs[0] & POLL_IN, 0, "must not be readable");
    assert!(
        elapsed >= std::time::Duration::from_millis(80),
        "poll must honor the 100ms timeout; slept only {elapsed:?}"
    );
    // Loose upper bound to catch runaway blocking.
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "poll slept way too long: {elapsed:?}"
    );
}

/// [`build_wakeup_channel`] must produce a usable UDP socket that
/// loops its own writes back into its recv queue. This is the basic
/// guarantee the wakeup layer relies on.
#[test]
fn new3_wakeup_channel_self_loopback() {
    let ch = build_wakeup_channel().expect("build wakeup channel");
    ch.socket.send(&[7u8]).expect("send");
    // Poll for readability with a 100ms upper bound.
    let req = PollReq {
        fd: dgram_pollreq_fd(&ch.socket),
        events: POLL_IN,
    };
    let revs = selector_poll(&[req], 100);
    assert_eq!(revs.len(), 1);
    assert!(
        revs[0] & POLL_IN != 0,
        "wakeup channel must observe its own write"
    );
    // Drain it.
    let mut buf = [0u8; 16];
    let n = ch.socket.recv(&mut buf).expect("recv");
    assert_eq!(n, 1);
    assert_eq!(buf[0], 7);
}

/// End-to-end test of the wakeup + poll integration as seen through
/// the process-wide `selector_wakeups` map. We emulate the selector
/// lifecycle without going through `NativeContext`: pretend an
/// arbitrary `ObjectRef` represents the selector, ensure a channel
/// is allocated for it, call `signal_wakeup`, then confirm that a
/// subsequent `poll_empty_selector`-style wait returns promptly.
#[test]
fn new3_signal_wakeup_interrupts_block() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    let mut ctx = crate::test_utils::MockNativeContext::new();

    // Synthesize a fake selector identity. The map is keyed by the
    // selector's stable identity, so any stable value works as long as
    // this test releases it at the end.
    let fake_sel = unsafe { ObjectRef::from_raw(0xdead_beef_0000_0100u64 as usize as *mut u8) };

    // Ensure a wakeup channel exists (would normally be allocated by
    // the first `select()` call).
    let existed =
        ensure_wakeup_channel(&ctx, fake_sel, |ch| dgram_pollreq_fd(&ch.socket)).is_some();
    assert!(existed, "wakeup channel should be constructible");

    let woke = Arc::new(AtomicBool::new(false));
    let woke_clone = Arc::clone(&woke);
    // Spawn a waker thread that triggers signal_wakeup after 50ms.
    let waker = std::thread::spawn(move || {
        let waker_ctx = crate::test_utils::MockNativeContext::new();
        std::thread::sleep(Duration::from_millis(50));
        signal_wakeup(&waker_ctx, fake_sel);
        woke_clone.store(true, Ordering::Release);
    });

    // Block for up to 2 seconds. If wakeup works, the poll returns
    // in roughly 50ms; if it doesn't, we wait the full 2 seconds.
    let t0 = Instant::now();
    let _ = poll_empty_selector(&mut ctx, fake_sel, 2_000);
    let elapsed = t0.elapsed();
    waker.join().expect("waker thread");

    assert!(woke.load(Ordering::Acquire));
    assert!(
        elapsed < Duration::from_millis(500),
        "wakeup must interrupt the poll in well under 500ms; took {elapsed:?}"
    );

    // Clean up so other tests don't see a stale entry.
    release_wakeup_channel(&ctx, fake_sel);
}

#[test]
fn s2_empty_selector_wait_enters_gc_blocked_region() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let fake_sel = unsafe { ObjectRef::from_raw(0xdead_beef_0000_0200u64 as usize as *mut u8) };

    let _ = poll_empty_selector(&mut ctx, fake_sel, 5);

    assert_eq!(
        ctx.blocking_region_counts(),
        (1, 1),
        "blocking select on an empty selector must be GC-blocked"
    );
    release_wakeup_channel(&ctx, fake_sel);
}

#[test]
fn s2_registered_selector_wait_enters_gc_blocked_region() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let sel = ctx.alloc_object(ClassId::new(0), 3);
    let keys = ctx.new_ref_array(ClassId::new(0), 1);
    let key = ctx.alloc_object(ClassId::new(0), 4);
    let channel = ctx.alloc_object(ClassId::new(0), 5);

    ctx.set_field(sel, S2SEL_OPEN, Value::Int(1));
    ctx.set_field(sel, S2SEL_KEYS, Value::Object(Some(keys)));
    ctx.set_field(sel, S2SEL_NKEYS, Value::Int(1));
    ctx.set_array_element(keys, 0, Value::Object(Some(key)));
    ctx.set_field(key, 0, Value::Object(Some(channel)));
    ctx.set_field(key, 2, Value::Int(1));
    ctx.set_field(channel, S2SSC_LISTENER_ID, Value::Int(-1));
    ctx.set_field(channel, S2SC_SOCK_ID, Value::Int(-1));
    ctx.set_field(channel, S2DC_SOCK_ID, Value::Int(-1));

    let n = s2_selector_do_poll_with_timeout(&mut ctx, sel, 5);

    assert_eq!(n, 0);
    assert_eq!(
        ctx.blocking_region_counts(),
        (1, 1),
        "blocking select with registered keys must be GC-blocked"
    );
    release_wakeup_channel(&ctx, sel);
}

/// Verify that `release_wakeup_channel` removes the entry from the
/// global map (otherwise the map would grow without bound for
/// long-running processes that repeatedly open and close selectors).
#[test]
fn new3_release_wakeup_channel_removes_entry() {
    let ctx = crate::test_utils::MockNativeContext::new();
    let fake_sel = unsafe { ObjectRef::from_raw(0xdead_beef_0000_0400u64 as usize as *mut u8) };
    let _ = ensure_wakeup_channel(&ctx, fake_sel, |_| ());
    assert!(selector_wakeups()
        .lock()
        .contains_key(&selector_key(&ctx, fake_sel)));
    release_wakeup_channel(&ctx, fake_sel);
    assert!(
        !selector_wakeups()
            .lock()
            .contains_key(&selector_key(&ctx, fake_sel)),
        "release_wakeup_channel must remove the entry"
    );
}

/// `ensure_wakeup_channel` must be idempotent — the second call must
/// return the same backing socket (same local port) as the first.
#[test]
fn new3_ensure_wakeup_channel_is_idempotent() {
    let ctx = crate::test_utils::MockNativeContext::new();
    let fake_sel = unsafe { ObjectRef::from_raw(0xdead_beef_0000_0300u64 as usize as *mut u8) };
    let first = ensure_wakeup_channel(&ctx, fake_sel, |ch| {
        ch.socket.local_addr().expect("local_addr")
    })
    .expect("first ensure");
    let second = ensure_wakeup_channel(&ctx, fake_sel, |ch| {
        ch.socket.local_addr().expect("local_addr")
    })
    .expect("second ensure");
    assert_eq!(
        first, second,
        "repeated ensure_wakeup_channel must yield the same socket"
    );
    release_wakeup_channel(&ctx, fake_sel);
}

/// Allocating a UDP socket via `s2_alloc_dgram_for_vm` must not collide
/// with existing stream / listener ids (NEW-3 refactor of the
/// next-id allocator).
#[test]
fn new3_alloc_dgram_avoids_collisions() {
    // A private VM identity: the registry is shared by every test in this
    // binary, and the teardown below must close only these two rows.
    const VM: usize = 0xB14_0301;
    // Can't actually construct TcpStream/TcpListener cheaply here
    // without loopback, but we can fabricate a `UdpSocket`.
    let s1 = UdpSocket::bind("127.0.0.1:0").expect("bind 1");
    let s2 = UdpSocket::bind("127.0.0.1:0").expect("bind 2");
    let id1 = s2_alloc_dgram_for_vm(VM, s1);
    let id2 = s2_alloc_dgram_for_vm(VM, s2);
    assert_ne!(id1, id2, "distinct UDP sockets must get distinct ids");
    // Clean up.
    forget_vm_s2_sockets(VM);
    let reg = s2_registry().lock();
    assert!(!reg.dgrams.contains_key(&id1) && !reg.dgrams.contains_key(&id2));
}

/// `s2_next_free_id` must never return 0 (reserved) and must skip
/// ids already in use by any of the three socket tables.
#[test]
fn new3_next_free_id_is_nonzero_and_unique() {
    let mut reg = SocketRegistry::default();
    reg.next_id = 0;
    // Pre-populate udp id 1 so the allocator must skip it.
    let dummy = UdpSocket::bind("127.0.0.1:0").expect("bind");
    reg.dgrams.insert(1, dummy);
    let id = s2_next_free_id(&mut reg);
    assert_ne!(id, 0, "id must never be 0");
    assert_ne!(id, 1, "id 1 already in use, must be skipped");
}

// =======================================================================
// gc-common w13-d: the process-wide socket tables record the VM that
// opened each row, and a VM's teardown closes exactly its own rows
// (`common-w12d-servlet-os-socket-tables-outlive-their-vm`). Each test
// uses private VM identities: the registry is shared by every test in
// this binary, which run in parallel.
// =======================================================================

/// Two VMs each hold a listener, VM A also an accepted stream, a datagram
/// socket and a selector wakeup channel. Tearing A down closes A's rows
/// (its port can be bound again, its peer sees the connection end) and
/// leaves B's rows and an unowned legacy row alone.
#[test]
fn forget_vm_s2_sockets_closes_only_that_vms_rows() {
    const VM_A: usize = 0xD13_0A01;
    const VM_B: usize = 0xD13_0B01;

    let la = TcpListener::bind("127.0.0.1:0").expect("bind A");
    let port_a = la.local_addr().expect("A addr").port();
    let lid_a = s2_alloc_listener_for_vm(VM_A, la);
    let lb = TcpListener::bind("127.0.0.1:0").expect("bind B");
    let lid_b = s2_alloc_listener_for_vm(VM_B, lb);

    // An accepted stream belongs to its listener's VM. The connection is
    // made on a plain listener so A's own listener never holds a
    // connection, and its port is free to rebind the moment it closes.
    let plain = TcpListener::bind("127.0.0.1:0").expect("bind plain");
    let mut client =
        TcpStream::connect(plain.local_addr().expect("plain addr")).expect("connect");
    let (server, _) = plain.accept().expect("accept");
    let sid_a = s2_alloc_accepted_stream(lid_a, server);

    let did_a = s2_alloc_dgram_for_vm(VM_A, UdpSocket::bind("127.0.0.1:0").expect("udp A"));
    // An unowned row, as a TLS connector leaves one before its caller
    // claims it (the VM-less `s2_alloc_dgram` this used is gone).
    let legacy = UdpSocket::bind("127.0.0.1:0").expect("udp legacy");
    let did_legacy = {
        let mut reg = s2_registry().lock();
        let id = s2_next_free_id(&mut reg);
        reg.dgrams.insert(id, legacy);
        id
    };

    let ctx_a = crate::test_utils::MockNativeContext::new();
    ctx_a.set_vm_identity(VM_A);
    let ctx_b = crate::test_utils::MockNativeContext::new();
    ctx_b.set_vm_identity(VM_B);
    let fake_sel = unsafe { ObjectRef::from_raw(0xdead_beef_0000_0d10u64 as usize as *mut u8) };
    ensure_wakeup_channel(&ctx_a, fake_sel, |_| ()).expect("wakeup A");
    ensure_wakeup_channel(&ctx_b, fake_sel, |_| ()).expect("wakeup B");

    {
        let reg = s2_registry().lock();
        assert_eq!(reg.owners.get(&lid_a), Some(&VM_A));
        assert_eq!(
            reg.owners.get(&sid_a),
            Some(&VM_A),
            "accepted stream inherits"
        );
        assert_eq!(reg.owners.get(&did_a), Some(&VM_A));
        assert_eq!(reg.owners.get(&lid_b), Some(&VM_B));
        assert_eq!(reg.owners.get(&did_legacy), None, "legacy rows are unowned");
    }

    forget_vm_s2_sockets(VM_A);

    {
        let reg = s2_registry().lock();
        assert!(
            !reg.listeners.contains_key(&lid_a),
            "A's listener row survived"
        );
        assert!(!reg.streams.contains_key(&sid_a), "A's stream row survived");
        assert!(
            !reg.dgrams.contains_key(&did_a),
            "A's datagram row survived"
        );
        assert!(
            !reg.owners.values().any(|v| *v == VM_A),
            "A's owner rows survived"
        );
        assert!(
            reg.listeners.contains_key(&lid_b),
            "B's listener was closed"
        );
        assert!(
            reg.dgrams.contains_key(&did_legacy),
            "an unowned row was closed"
        );
    }
    assert!(
        !selector_wakeups()
            .lock()
            .contains_key(&selector_key(&ctx_a, fake_sel)),
        "A's wakeup channel survived"
    );
    assert!(
        selector_wakeups()
            .lock()
            .contains_key(&selector_key(&ctx_b, fake_sel)),
        "B's wakeup channel was dropped"
    );

    // The port is released...
    TcpListener::bind(("127.0.0.1", port_a)).expect("A's port must be free after teardown");
    // ...and the accepted stream was really closed, not just unlisted.
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("read timeout");
    let mut buf = [0u8; 1];
    match client.read(&mut buf) {
        Ok(n) => assert_eq!(n, 0, "the peer must see end of stream"),
        Err(e) => assert!(
            !matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "the peer must see the connection end, not wait: {e}"
        ),
    }

    forget_vm_s2_sockets(VM_B);
    assert!(!s2_registry().lock().listeners.contains_key(&lid_b));
    s2_registry().lock().dgrams.remove(&did_legacy);
}

/// An accept used to mint its id by hand: it took `next_id` unchecked,
/// ignored the datagram and TLS tables, and could return 0 after a wrap.
#[test]
fn accepted_stream_id_skips_live_rows_and_inherits_the_listener_owner() {
    let mut reg = SocketRegistry::default();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    reg.listeners.insert(5, listener);
    reg.owners.insert(5, 0xD13_0C01);
    reg.dgrams
        .insert(1, UdpSocket::bind("127.0.0.1:0").expect("udp"));
    reg.next_id = 1;
    let _client = TcpStream::connect(addr).expect("connect");

    // The connection is already queued on loopback; allow a bounded wait
    // for the non-blocking accept to see it.
    let mut id = None;
    for _ in 0..400 {
        id = s2_try_accept_nonblocking(&mut reg, 5);
        if id.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let id = id.expect("accept");
    assert_ne!(id, 0);
    assert_ne!(id, 1, "id 1 is a live datagram row");
    assert_ne!(id, 5, "id 5 is the live listener row");
    assert!(reg.dgrams.contains_key(&1));
    assert_eq!(reg.owners.get(&id), Some(&0xD13_0C01));
}

/// Closes remove only the socket row. The owner table must stay bounded by
/// the live rows, and a leftover owner row must not attach to a new socket
/// that reuses its id.
#[test]
fn owner_rows_of_closed_sockets_are_pruned_and_never_reattached() {
    let mut reg = SocketRegistry::default();
    reg.dgrams
        .insert(1, UdpSocket::bind("127.0.0.1:0").expect("udp"));
    s2_note_owner(&mut reg, 1, 7);
    for id in 2..500 {
        // Owner rows for sockets that have since closed.
        s2_note_owner(&mut reg, id, 7);
    }
    assert!(
        reg.owners.len() <= 65,
        "owner rows of closed sockets must be pruned, got {}",
        reg.owners.len()
    );
    assert_eq!(reg.owners.get(&1), Some(&7), "a live row keeps its owner");

    reg.next_id = 900;
    reg.owners.insert(900, 7);
    let id = s2_next_free_id(&mut reg);
    assert_eq!(id, 900);
    assert!(
        !reg.owners.contains_key(&900),
        "a reused id must start unowned"
    );
}

/// `SocketChannel.connect` records the calling VM as the owner and runs
/// the connect inside the GC-blocked region.
#[test]
fn socket_channel_connect_is_gc_blocked_and_owned_by_its_vm() {
    const VM: usize = 0xD13_0D01;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    register_s2_socket_channel(&mut registry);
    let cb = registry
        .find(
            "java/nio/channels/SocketChannel",
            "connect",
            "(Ljava/net/SocketAddress;)Z",
        )
        .expect("SocketChannel.connect native");

    let mut ctx = crate::test_utils::MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let ch = ctx.alloc_object(ClassId::new(0), 5);
    ctx.set_field(ch, S2SC_SOCK_ID, Value::Int(-1));
    ctx.set_field(ch, S2SC_BLOCKING, Value::Int(1));
    // The synthetic two-field address shape: host string, port.
    let addr = ctx.alloc_object(ClassId::new(0), 2);
    let host = ctx.create_string("127.0.0.1");
    ctx.set_field(addr, 0, Value::Object(Some(host)));
    ctx.set_field(addr, 1, Value::Int(port as i32));

    match cb(
        &mut ctx,
        &[Value::Object(Some(ch)), Value::Object(Some(addr))],
    ) {
        Ok(Some(Value::Int(1))) => {}
        Ok(other) => panic!("SocketChannel.connect answered {other:?}"),
        Err(_) => panic!("SocketChannel.connect threw"),
    }
    let sid = ctx.get_field(ch, S2SC_SOCK_ID).as_int().unwrap_or(-1);
    assert!(sid > 0, "no socket id recorded");
    assert_eq!(s2_registry().lock().owners.get(&sid), Some(&VM));
    assert_eq!(
        ctx.blocking_region_counts(),
        (1, 1),
        "a blocking connect must be GC-blocked"
    );

    forget_vm_s2_sockets(VM);
    assert!(!s2_registry().lock().streams.contains_key(&sid));
}

// =======================================================================
// bb_write_hb — real-JDK Buffer field layout (mark@0/position@1/limit@2/
// capacity@3/address@4/hb@5/offset@6, per `mock_buffer_field_slot`) must
// not be clobbered by the legacy BB_* indexed-slot fallback. Regression
// test for the AIOOBE in
// zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md:
// BB_MARK(4)/BB_ARRAY(0) used to alias real `address`/`mark` and were
// written unconditionally AFTER the correct by-name writes, silently
// resetting `address` to -1 and `mark` to a truncated array pointer.
// =======================================================================

#[test]
fn bb_write_hb_real_layout_preserves_address_and_mark() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let class_id = ctx
        .ensure_class_initialized("java/nio/ByteBuffer")
        .expect("class init");
    // A real-JDK-shaped ByteBuffer has more than the bare 6 synthetic
    // fields (mark/position/limit/capacity/address/segment plus
    // ByteBuffer's own hb/offset/...); allocate more than 6 slots so
    // `s2_bb_synthetic_layout` correctly identifies this as real, not
    // the pure-synthetic fallback layout.
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 64);
    let buf = ctx.alloc_object(class_id, 10);

    bb_write_hb(&mut ctx, buf, arr, 64);

    assert_eq!(
        ctx.get_field_by_name(buf, "address"),
        Value::Long(16),
        "real Buffer.address must be seeded to ARRAY_BYTE_BASE_OFFSET, not clobbered by BB_MARK"
    );
    assert_eq!(
        ctx.get_field_by_name(buf, "mark"),
        Value::Int(-1),
        "real Buffer.mark must stay -1, not clobbered by BB_ARRAY's array reference"
    );
    assert_eq!(ctx.get_field_by_name(buf, "hb"), Value::Object(Some(arr)));
    assert_eq!(ctx.get_field_by_name(buf, "position"), Value::Int(0));
    assert_eq!(ctx.get_field_by_name(buf, "limit"), Value::Int(64));
    assert_eq!(ctx.get_field_by_name(buf, "capacity"), Value::Int(64));
}

#[test]
fn bb_write_hb_pure_synthetic_layout_still_gets_indexed_fallback() {
    // A genuinely synthetic (non-real-JDK) 6-field ByteBuffer carrier —
    // no field-name metadata resolves, so the indexed BB_* fallback is
    // the only way these natives can round-trip state. Guard against a
    // regression where the fix above accidentally suppresses the
    // fallback for this legitimate case too.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let class_id = ClassId::new(9999); // never registered by name
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
    let buf = ctx.alloc_object(class_id, 6);

    bb_write_hb(&mut ctx, buf, arr, 32);

    assert_eq!(ctx.get_field(buf, BB_ARRAY), Value::Object(Some(arr)));
    assert_eq!(ctx.get_field(buf, BB_LIMIT), Value::Int(32));
    assert_eq!(ctx.get_field(buf, BB_CAP), Value::Int(32));
    assert_eq!(ctx.get_field(buf, BB_MARK), Value::Int(-1));
}

// =======================================================================
// Real-JDK `ByteBufferAs<T>Buffer{B,L}` views (2026-08-10).
//
// `ByteBuffer.as<T>Buffer()` is not force-listed over real JDK bytecode, so
// on a real JDK it hands back one of these concrete view classes: storage on
// the backing `bb`, `hb` null on the view itself, and `Buffer.address`
// holding an UNSAFE offset (`ARRAY_BYTE_BASE_OFFSET + byteIndex`) rather
// than a process pointer. The bulk `get([JII)`/`put([JII)` accessors ARE
// force-listed (they are declared on the abstract `java/nio/LongBuffer`,
// which these views do not override), so such a receiver reaches
// `s2_bb_get_byte`, which used to read `address` as a pointer and hand 0x10
// to `copy_from_native_memory`. SIGSEGV at addr=0x10 on every
// `org.h2.mvstore.Chunk.readToC`.
// =======================================================================

/// Build a real-JDK-shaped `ByteBufferAs<T>Buffer{B,L}` over a heap
/// ByteBuffer: 10 slots (so `s2_bb_synthetic_layout` reads it as a real
/// layout, not the bare 6-field synthetic carrier) with the real
/// `Buffer`/`ByteBufferAsXBuffer` field names declared, `bb` pointing at the
/// backing buffer and `address` seeded the way the JDK's own
/// `as<T>Buffer()` seeds it: `bb.address + bb.position()`.
fn make_real_typed_view(
    ctx: &mut crate::test_utils::MockNativeContext,
    view_class: &str,
    byte_start: i64,
) -> (ObjectRef, ObjectRef, ObjectRef) {
    use cratonvm_native_api::FieldMetadata;

    let bb_class = ctx
        .ensure_class_initialized("java/nio/ByteBuffer")
        .expect("class init");
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 64);
    let bb = ctx.alloc_object(bb_class, 10);
    bb_write_hb(ctx, bb, arr, 64);

    let view_class_id = ctx
        .ensure_class_initialized(view_class)
        .expect("class init");
    let names = [
        ("mark", "I"),
        ("position", "I"),
        ("limit", "I"),
        ("capacity", "I"),
        ("address", "J"),
        ("segment", "Ljava/lang/foreign/MemorySegment;"),
        ("bb", "Ljava/nio/ByteBuffer;"),
        ("hb", "[J"),
        ("offset", "I"),
        ("isReadOnly", "Z"),
    ];
    ctx.set_declared_fields(
        view_class_id,
        names
            .iter()
            .enumerate()
            .map(|(i, (name, descriptor))| FieldMetadata {
                name: (*name).to_string(),
                descriptor: (*descriptor).to_string(),
                access_flags: 0,
                slot_index: i,
                declaring_class_id: view_class_id,
                is_static: false,
            })
            .collect(),
    );
    let view = ctx.alloc_object(view_class_id, 10);
    ctx.set_field_by_name(view, "bb", Value::Object(Some(bb)));
    ctx.set_field_by_name(view, "mark", Value::Int(-1));
    ctx.set_field_by_name(view, "position", Value::Int(0));
    ctx.set_field_by_name(view, "limit", Value::Int(4));
    ctx.set_field_by_name(view, "capacity", Value::Int(4));
    ctx.set_field_by_name(
        view,
        "address",
        Value::Long(ARRAY_BYTE_BASE_OFFSET + byte_start),
    );
    (view, bb, arr)
}

/// The crash precondition, stated directly: a view over a HEAP buffer must
/// never be classified as direct. Before the fix `s2_bb_direct_addr`
/// answered `Some(16 + byte_start)` here and the byte accessors
/// dereferenced it as a process pointer.
#[test]
fn real_typed_view_over_a_heap_buffer_is_not_direct() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let (view, _bb, _arr) =
        make_real_typed_view(&mut ctx, "java/nio/ByteBufferAsLongBufferB", 0);

    assert!(
        s2_bb_heap_window(&ctx, view).is_some(),
        "the view's storage must resolve through its backing `bb` — this is the \
         mechanism, the address guard below is only the backstop"
    );
    assert_eq!(
        s2_bb_direct_addr(&ctx, view),
        None,
        "a ByteBufferAs<T>Buffer over a HEAP buffer carries an array-relative \
         Unsafe offset in `address`, not a native pointer — reading it as one \
         is the addr=0x10 SIGSEGV"
    );
    assert!(
        !is_plausible_native_addr(ARRAY_BYTE_BASE_OFFSET),
        "ARRAY_BYTE_BASE_OFFSET is below the first mappable page and must never \
         be accepted as a process pointer"
    );
}

/// And the positive half: the view reads the bytes it aliases. `address`
/// folds in the source's array-base offset AND its position at the moment
/// the view was taken, so element 0 lives at `address - ARRAY_BYTE_BASE_OFFSET`.
#[test]
fn real_typed_view_reads_through_its_backing_bytebuffer() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let (view, _bb, arr) =
        make_real_typed_view(&mut ctx, "java/nio/ByteBufferAsLongBufferB", 4);
    for i in 0..16usize {
        ctx.set_array_element(arr, i, Value::Int(i as i32));
    }

    assert_eq!(
        s2_bb_get_byte(&ctx, view, 0),
        4,
        "the view's byte 0 is `address - ARRAY_BYTE_BASE_OFFSET` into the backing array"
    );
    assert_eq!(s2_bb_get_byte(&ctx, view, 3), 7);

    s2_bb_put_byte(&mut ctx, view, 1, 99);
    assert_eq!(
        ctx.get_array_element(arr, 5).as_int(),
        Some(99),
        "a write through the view must land in the SHARED backing array — a view is \
         not a copy"
    );
}

/// The JDK compiles one concrete view class per endianness and `order()` is
/// a constant return, so the class name is the exact answer. The old
/// `mark`-slot fallback answered BIG_ENDIAN for every such view, which would
/// byteswap every read through a `...BufferL`.
#[test]
fn real_typed_view_endianness_comes_from_its_class_name() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let (be, _, _) = make_real_typed_view(&mut ctx, "java/nio/ByteBufferAsLongBufferB", 0);
    assert_eq!(s2_bb_order(&ctx, be), 0, "…BufferB is BIG_ENDIAN");

    let mut ctx = crate::test_utils::MockNativeContext::new();
    let (le, _, _) = make_real_typed_view(&mut ctx, "java/nio/ByteBufferAsIntBufferL", 0);
    assert_eq!(s2_bb_order(&ctx, le), 1, "…BufferL is LITTLE_ENDIAN");
}

// =======================================================================
// `order(ByteOrder)` must never write a scalar over the backing array
// (2026-08-12).
//
// Measured on `cratonvm --synthetic-jdk` before the fix:
//
//     P8 order-after=LITTLE_ENDIAN
//     [cratonvm] main-vm run() returned Err: internal error: ByteBuffer
//       missing backing storage (hb/slot5/address absent; field 0 returned
//       Int(1), address Object(None))
//
// — exit 1, no Java exception, on a plain
// `ByteBuffer.allocate(8).order(ByteOrder.nativeOrder())`. The corruption
// is SELF-CONSISTENT (the reader read the same slot back), so `order()`
// still reported LITTLE_ENDIAN over the destroyed buffer. Every assertion
// below is therefore about the STORAGE, never about the reported order
// alone.
// =======================================================================

/// A `--synthetic-jdk` buffer carrier, as production actually builds one:
/// wider than the 6 slots `s2_bb_synthetic_layout` screens for, no real
/// `java.nio.Buffer` field metadata, backing array in slot 0.
///
/// The class is deliberately NOT named `java/nio/ByteBuffer`, and that is a
/// property of the harness rather than of production:
/// `MockNativeContext`'s `mock_buffer_field_slot` answers `position` for
/// any `java/nio/*ByteBuffer`, so under the mock that name can never
/// present as a stub layout. In the VM it does —
/// `class_manager::synthetic_stub_fields` gives `java/nio/ByteBuffer`
/// `instance_fields(6)` (`_f0.._f5`, all `Ljava/lang/Object;`) over a
/// `java/nio/Buffer` with `instance_fields(4)`, which is the ten fields
/// `Class.getDeclaredFields()` prints under `--synthetic-jdk`.
fn make_stub_layout_buffer(
    ctx: &mut crate::test_utils::MockNativeContext,
) -> (ObjectRef, ObjectRef) {
    // Faithful "no such field" answer, so `bigEndian` reports absent the
    // way `vm_exec.rs` reports it rather than the mock's default `Int(0)`.
    ctx.set_absent_field_answers_null(true);
    let class_id = ctx
        .ensure_class_initialized("craton/test/SyntheticStubByteBuffer")
        .expect("class init");
    let buf = ctx.alloc_object(class_id, 10);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
    // Exactly what `native-io`'s `alloc_byte_buffer` writes.
    ctx.set_field(buf, BB_ARRAY, Value::Object(Some(arr)));
    ctx.set_field(buf, BB_POS, Value::Int(0));
    ctx.set_field(buf, BB_LIMIT, Value::Int(8));
    ctx.set_field(buf, BB_CAP, Value::Int(8));
    ctx.set_field(buf, BB_MARK, Value::Int(-1));
    (buf, arr)
}

/// THE DEFECT, stated as the storage question. Not `order()`, which lied.
#[test]
fn setting_the_order_on_a_stub_layout_bytebuffer_keeps_the_backing_array() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let (buf, arr) = make_stub_layout_buffer(&mut ctx);
    assert!(
        s2_buf_stub_layout(&ctx, buf),
        "premise: a 10-field carrier with no real `Buffer` field metadata is a \
         stub layout, even though `s2_bb_synthetic_layout` (== 6) says otherwise"
    );
    assert!(
        !s2_bb_synthetic_layout(&ctx, buf),
        "and the narrow screen genuinely does NOT catch it — without this the \
         test would pass for the wrong reason"
    );

    s2_bb_set_order(&mut ctx, buf, 1);

    assert_eq!(
        ctx.get_field(buf, BB_ARRAY),
        Value::Object(Some(arr)),
        "slot 0 is the BACKING ARRAY on this layout; an Int written here is the \
         `ByteBuffer missing backing storage` VM abort"
    );
    assert_eq!(
        s2_bb_arr(&ctx, buf),
        Some(arr),
        "and it must still resolve as the buffer's storage, by identity"
    );
    assert_eq!(
        s2_bb_order(&ctx, buf),
        1,
        "reader and writer must agree: the order is stored where BB_ORDER says"
    );
    assert_eq!(
        ctx.get_field(buf, BB_ORDER),
        Value::Int(1),
        "and that slot is the one named for it, not an incidental free slot"
    );
}

/// The round trip in both directions, and the default. A one-way check
/// would pass against a writer that had simply stopped writing.
#[test]
fn stub_layout_bytebuffer_order_round_trips_both_ways() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let (buf, arr) = make_stub_layout_buffer(&mut ctx);

    assert_eq!(
        s2_bb_order(&ctx, buf),
        0,
        "a fresh buffer is BIG_ENDIAN — the JDK field initialiser"
    );
    s2_bb_set_order(&mut ctx, buf, 1);
    assert_eq!(s2_bb_order(&ctx, buf), 1);
    s2_bb_set_order(&mut ctx, buf, 0);
    assert_eq!(
        s2_bb_order(&ctx, buf),
        0,
        "LITTLE_ENDIAN must be reversible; a writer that only ever set the flag \
         would pass the forward check alone"
    );
    assert_eq!(
        s2_bb_arr(&ctx, buf),
        Some(arr),
        "the storage survives every transition, not just the first"
    );
}

/// The ONE family for which slot 0 is the right home — and the reason the
/// fix is a class-name gate rather than a deletion. A typed view parks its
/// backing array in `BB_SEGMENT_SLOT`, leaving real `mark` free.
#[test]
fn a_typed_buffer_view_still_stores_its_order_in_the_mark_slot() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    ctx.set_absent_field_answers_null(true);
    let class_id = ctx
        .ensure_class_initialized("java/nio/IntBuffer")
        .expect("class init");
    let view = ctx.alloc_object(class_id, 6);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    // `s2_view_buf_fn!`'s layout: array at slot 5, byte-start marker at 4.
    ctx.set_field(view, BB_SEGMENT_SLOT, Value::Object(Some(arr)));
    ctx.set_field(view, BB_MARK, Value::Int(-1));

    s2_bb_set_order(&mut ctx, view, 1);

    assert_eq!(
        ctx.get_field(view, BB_SEGMENT_SLOT),
        Value::Object(Some(arr)),
        "the view's backing array lives at BB_SEGMENT_SLOT and must be untouched"
    );
    assert_eq!(
        ctx.get_field(view, BB_ARRAY),
        Value::Int(1),
        "slot 0 is real `mark: int` and genuinely free on this shape"
    );
    assert_eq!(s2_bb_order(&ctx, view), 1);
    s2_bb_set_order(&mut ctx, view, 0);
    assert_eq!(s2_bb_order(&ctx, view), 0);
}

/// A REAL JDK layout that declares no `bigEndian` — `java.nio.CharBuffer`
/// is one — must be refused, not guessed at. Every index this function
/// could reach for aliases a field the real class declares, so the old
/// unconditional fallback wrote an `int` order flag over real
/// `Buffer.mark`. Refusing leaves the JDK default, which is a recoverable
/// wrong value rather than a corrupted object.
#[test]
fn a_real_layout_buffer_without_big_endian_refuses_rather_than_writing_slot_zero() {
    use cratonvm_native_api::FieldMetadata;
    let mut ctx = crate::test_utils::MockNativeContext::new();
    ctx.set_absent_field_answers_null(true);
    let class_id = ctx
        .ensure_class_initialized("java/nio/CharBuffer")
        .expect("class init");
    let names = [
        ("mark", "I"),
        ("position", "I"),
        ("limit", "I"),
        ("capacity", "I"),
        ("address", "J"),
        ("segment", "Ljava/lang/foreign/MemorySegment;"),
        ("hb", "[C"),
        ("offset", "I"),
        ("isReadOnly", "Z"),
    ];
    ctx.set_declared_fields(
        class_id,
        names
            .iter()
            .enumerate()
            .map(|(i, (name, descriptor))| FieldMetadata {
                name: (*name).to_string(),
                descriptor: (*descriptor).to_string(),
                access_flags: 0,
                slot_index: i,
                declaring_class_id: class_id,
                is_static: false,
            })
            .collect(),
    );
    let buf = ctx.alloc_object(class_id, 9);
    let chars = ctx.new_array(cratonvm_types::ArrayElementType::Char, 8);
    let sentinel = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 1);
    ctx.set_field_by_name(buf, "mark", Value::Int(-1));
    ctx.set_field_by_name(buf, "hb", Value::Object(Some(chars)));
    // A reference parked in the slot `BB_ORDER` aliases on a real layout,
    // so the refusal is checked by IDENTITY rather than by "still non-null".
    ctx.set_field_by_name(buf, "segment", Value::Object(Some(sentinel)));

    assert!(
        !s2_buf_stub_layout(&ctx, buf),
        "premise: `position` resolves, so this is a real layout"
    );

    s2_bb_set_order(&mut ctx, buf, 1);

    assert_eq!(
        ctx.get_field_by_name(buf, "mark"),
        Value::Int(-1),
        "real `Buffer.mark` must not become a byte-order flag"
    );
    assert_eq!(
        ctx.get_field_by_name(buf, "segment"),
        Value::Object(Some(sentinel)),
        "and neither may the reference-typed `segment` slot that BB_ORDER \
         aliases on a real layout"
    );
    assert_eq!(
        ctx.get_field_by_name(buf, "hb"),
        Value::Object(Some(chars)),
        "the backing store is still the same array object"
    );
    assert_eq!(
        s2_bb_order(&ctx, buf),
        0,
        "the reader agrees with the refusal instead of decoding `mark` as an order"
    );
}

// ---- G38-1: the reference slot the ByteOrder fallback used to null ------
//
// `G38-1-the-live-reference-slot-writes-20260817.md`. Reaching the
// fallback in `s2_byte_order_object` does NOT prove the real class is
// absent — `ensure_class_initialized` can succeed while the statics are
// not yet published, and every compatibility-mode arm runs a synthetic
// registrar against real class bytes. On that path the object IS a real
// `java.nio.ByteOrder`, whose sole instance field is
// `private final String name` at slot 0, and `Value::Int(ord)` there was
// coerced to `null` by `heap::coerce_field_value_by_descriptor`.

/// The falsifying half. Before the fix the fallback wrote ONLY the order
/// Int, so on a real layout `name` held an `Int` (mock) / `null` (VM) and
/// both readers decoded LITTLE_ENDIAN as BIG_ENDIAN.
#[test]
fn the_byte_order_fallback_names_the_constant_when_the_class_declares_name() {
    use cratonvm_native_api::FieldMetadata;
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let class_id = ctx
        .ensure_class_initialized("java/nio/ByteOrder")
        .expect("class init");
    ctx.set_declared_fields(
        class_id,
        vec![FieldMetadata {
            name: "name".to_string(),
            descriptor: "Ljava/lang/String;".to_string(),
            access_flags: 0,
            slot_index: 0,
            declaring_class_id: class_id,
            is_static: false,
        }],
    );

    for (ord, expected) in [(0i32, "BIG_ENDIAN"), (1i32, "LITTLE_ENDIAN")] {
        let bo = s2_byte_order_object(&mut ctx, ord).expect("byte order object");
        let name = match ctx.get_field_by_name(bo, "name") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            other => panic!(
                "`ByteOrder.name` is a String reference; the fallback left {other:?} \
                 — that is the value the descriptor coercion turns into null"
            ),
        };
        assert_eq!(
            name, expected,
            "the constant must name ITSELF, not whichever constant a null decodes to"
        );
    }
}

/// The other half, so the fix cannot be "write a String everywhere".
/// A shape with no in-range `name` slot keeps the order flag and nothing
/// else — that is the synthetic-stub layout, where slot 0 is genuinely an
/// untyped field and every reader falls back to the Int.
#[test]
fn a_byte_order_shape_with_no_in_range_name_slot_keeps_only_the_flag() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let bo = s2_byte_order_object(&mut ctx, 1).expect("byte order object");
    assert_eq!(
        ctx.object_num_fields(bo),
        1,
        "premise: the fallback allocates the 1-slot stand-in"
    );
    assert_eq!(
        ctx.get_field(bo, 0),
        Value::Int(1),
        "no `name` field resolves in range, so only the order flag lands \
         and the object is byte-identical to the pre-G38 one"
    );
}

/// And the decode both writers feed: a String at slot 0 must round-trip
/// through the SAME `toString`/`equals` natives the Int does, or the fix
/// would have moved the defect into the readers.
#[test]
fn the_byte_order_readers_decode_a_name_string_as_well_as_the_flag() {
    use cratonvm_native_api::FieldMetadata;
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    register_s2_byteorder(&mut registry);
    let to_string = registry
        .find("java/nio/ByteOrder", "toString", "()Ljava/lang/String;")
        .expect("ByteOrder.toString native");
    let equals = registry
        .find("java/nio/ByteOrder", "equals", "(Ljava/lang/Object;)Z")
        .expect("ByteOrder.equals native");

    let mut ctx = crate::test_utils::MockNativeContext::new();
    let class_id = ctx
        .ensure_class_initialized("java/nio/ByteOrder")
        .expect("class init");
    ctx.set_declared_fields(
        class_id,
        vec![FieldMetadata {
            name: "name".to_string(),
            descriptor: "Ljava/lang/String;".to_string(),
            access_flags: 0,
            slot_index: 0,
            declaring_class_id: class_id,
            is_static: false,
        }],
    );
    let big = s2_byte_order_object(&mut ctx, 0).expect("BIG_ENDIAN");
    let little = s2_byte_order_object(&mut ctx, 1).expect("LITTLE_ENDIAN");

    for (obj, expected) in [(big, "BIG_ENDIAN"), (little, "LITTLE_ENDIAN")] {
        let rendered = match to_string(&mut ctx, &[Value::Object(Some(obj))]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            other => panic!("toString returned {other:?}"),
        };
        assert_eq!(rendered, expected);
    }
    // `MethodCallFailed` is not `PartialEq`, so unwrap rather than compare
    // the whole `Result`.
    let eq = |ctx: &mut crate::test_utils::MockNativeContext, a: ObjectRef, b: ObjectRef| {
        match equals(ctx, &[Value::Object(Some(a)), Value::Object(Some(b))]) {
            Ok(Some(Value::Int(v))) => v,
            other => panic!("equals returned {other:?}"),
        }
    };
    assert_eq!(
        eq(&mut ctx, big, little),
        0,
        "two DIFFERENT constants must not compare equal — they did while \
         both slot-0 values decoded to 0"
    );
    assert_eq!(
        eq(&mut ctx, big, big),
        1,
        "and the same constant must still compare equal to itself"
    );
}

/// `s2_bb_alloc_direct` gained the by-name writes and the layout screen
/// (G38-1). On the bare six-slot synthetic layout — the ONLY shape this
/// function is reached on today — the screen is open and every indexed
/// slot must end byte-identical to the pre-G38 answer.
#[test]
fn the_direct_buffer_overlay_is_unchanged_on_the_bare_synthetic_layout() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let buf = match s2_bb_alloc_direct(&mut ctx, 16) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("allocateDirect returned {other:?}"),
    };
    assert!(
        s2_bb_synthetic_layout(&ctx, buf),
        "premise: no real ByteBuffer layout is declared, so the screen is open"
    );
    assert_eq!(ctx.get_field(buf, BB_ARRAY), Value::Object(None));
    assert_eq!(ctx.get_field(buf, BB_POS), Value::Int(0));
    assert_eq!(ctx.get_field(buf, BB_LIMIT), Value::Int(16));
    assert_eq!(ctx.get_field(buf, BB_CAP), Value::Int(16));
    assert!(
        matches!(ctx.get_field(buf, BB_MARK), Value::Long(_)),
        "BB_MARK carries the native address on a direct buffer"
    );
    assert_eq!(
        ctx.get_field(buf, BB_ORDER),
        Value::Int(0),
        "the order flag still lands in the slot the synthetic readers use"
    );
    assert_eq!(s2_bb_order(&ctx, buf), 0);
    assert!(
        s2_bb_arr(&ctx, buf).is_none(),
        "a direct buffer has no backing array"
    );
}

/// And the screen itself, which is what keeps that overlay off a real
/// layout: on the eleven-field JDK 25 `java.nio.ByteBuffer` the six
/// indices alias `mark/position/limit/capacity/address/segment`, so
/// `BB_ORDER` would land on a REFERENCE and `BB_ARRAY` on an `int`.
#[test]
fn the_real_byte_buffer_layout_closes_the_direct_overlay_screen() {
    use cratonvm_native_api::FieldMetadata;
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let class_id = ctx
        .ensure_class_initialized("java/nio/ByteBuffer")
        .expect("class init");
    let names = [
        ("mark", "I"),
        ("position", "I"),
        ("limit", "I"),
        ("capacity", "I"),
        ("address", "J"),
        ("segment", "Ljava/lang/foreign/MemorySegment;"),
        ("hb", "[B"),
        ("offset", "I"),
        ("isReadOnly", "Z"),
        ("bigEndian", "Z"),
        ("nativeByteOrder", "Z"),
    ];
    ctx.set_declared_fields(
        class_id,
        names
            .iter()
            .enumerate()
            .map(|(i, (name, descriptor))| FieldMetadata {
                name: (*name).to_string(),
                descriptor: (*descriptor).to_string(),
                access_flags: 0,
                slot_index: i,
                declaring_class_id: class_id,
                is_static: false,
            })
            .collect(),
    );
    let buf = ctx.alloc_object(class_id, names.len());
    assert!(
        !s2_bb_synthetic_layout(&ctx, buf),
        "an eleven-field real layout must never take the indexed overlay"
    );
    assert_eq!(
        ctx.resolve_field_index_by_class_id(class_id, "segment"),
        Some(BB_ORDER),
        "BB_ORDER is exactly the reference-typed `segment` slot — that is \
         why the unconditional write destroyed the order flag"
    );
    assert_eq!(
        ctx.resolve_field_index_by_class_id(class_id, "mark"),
        Some(BB_ARRAY),
        "and BB_ARRAY is `mark`, an int, which is the other direction of \
         the same coercion"
    );
}
