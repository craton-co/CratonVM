use super::*;
use crate::test_support::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn make_bais(ctx: &mut MockNativeContext, bytes: &[u8]) -> (ObjectRef, ObjectRef) {
    let buf = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(buf, i, Value::Int(*b as i32));
    }
    let this = ctx.alloc_object_with_class(4, "java/io/ByteArrayInputStream");
    native_bais_init(ctx, &[Value::Object(Some(this)), Value::Object(Some(buf))])
        .expect("init ok");
    (this, buf)
}

#[test]
fn init_writes_count_to_slot_3_not_slot_2() {
    let mut ctx = MockNativeContext::new();
    let (this, _) = make_bais(&mut ctx, b"hello world");
    assert_eq!(ctx.get_field(this, BAIS_FIELD_MARK), Value::Int(0));
    assert_eq!(ctx.get_field(this, BAIS_FIELD_COUNT), Value::Int(11));
    assert_eq!(ctx.get_field(this, BAIS_FIELD_POS), Value::Int(0));
}

#[test]
fn read_byte_advances_pos_and_returns_unsigned() {
    let mut ctx = MockNativeContext::new();
    let (this, _) = make_bais(&mut ctx, b"Hi");
    let a = native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    assert_eq!(a, Some(Value::Int(b'H' as i32)));
    let b = native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    assert_eq!(b, Some(Value::Int(b'i' as i32)));
    let eof = native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    assert_eq!(eof, Some(Value::Int(-1)));
}

#[test]
fn read_bytes_bulk_returns_length_not_minus_one() {
    let mut ctx = MockNativeContext::new();
    let (this, _) = make_bais(&mut ctx, b"hello world");
    let dst = ctx.new_array(ArrayElementType::Byte, 32);
    let n = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(32),
        ],
    )
    .unwrap();
    assert_eq!(n, Some(Value::Int(11)));
    let mut got = Vec::new();
    for i in 0..11 {
        if let Value::Int(b) = ctx.get_array_element(dst, i) {
            got.push(b as u8);
        }
    }
    assert_eq!(&got, b"hello world");
}

#[test]
fn offset_constructor_negative_length_is_immediate_eof_for_bulk_reads() {
    let mut ctx = MockNativeContext::new();
    let buf = ctx.new_array(ArrayElementType::Byte, 4);
    let this = ctx.alloc_object_with_class(4, "java/io/ByteArrayInputStream");
    native_bais_init_offset(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(buf)),
            Value::Int(0),
            Value::Int(-1),
        ],
    )
    .expect("init ok");
    let dst = ctx.new_array(ArrayElementType::Byte, 8);

    let n = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(8),
        ],
    )
    .expect("read ok");
    assert_eq!(n, Some(Value::Int(-1)));

    // EOF wins over the zero-length short-circuit, exactly as the JDK
    // orders the two checks. Witnessed on stock HotSpot JDK 25:
    //   new ByteArrayInputStream(new byte[4], 0, -1) => count = -1
    //   read(dst, 0, 8) == -1   read(dst, 0, 0) == -1   available() == -1
    // This assertion used to read `Int(0)`, which froze the divergence
    // netty's AbstractByteBufTest.testStreamTransfer1 tripped over.
    let zero_length = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(0),
        ],
    )
    .expect("zero-length read ok");
    assert_eq!(zero_length, Some(Value::Int(-1)));
}

fn make_hibernate_lob_stream(ctx: &mut MockNativeContext, count: i64) -> ObjectRef {
    let owner = ctx.alloc_object(1);
    let count_obj = ctx.alloc_object_with_class(1, "java/lang/Long");
    ctx.set_field(count_obj, 0, Value::Long(count));
    let stream = ctx.alloc_object_with_class(
        0,
        "org/hibernate/orm/test/lob/JpaLargeBlobTest$LobInputStream",
    );
    ctx.set_field_by_name(stream, "read", Value::Int(0));
    ctx.set_field_by_name(stream, "this$0", Value::Object(Some(owner)));
    ctx.set_field_by_name(stream, "count", Value::Object(Some(count_obj)));
    stream
}

#[test]
fn hibernate_lob_stream_bulk_read_decrements_count_without_byte_loop() {
    let mut ctx = MockNativeContext::new();
    let stream = make_hibernate_lob_stream(&mut ctx, 13);
    let dst = ctx.new_array(ArrayElementType::Byte, 16);
    ctx.script("read", "()I", Ok(Some(Value::Int(123))));

    let n = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(stream)),
            Value::Object(Some(dst)),
            Value::Int(2),
            Value::Int(8),
        ],
    )
    .unwrap();

    assert_eq!(n, Some(Value::Int(8)));
    assert!(
        ctx.recorded_calls().is_empty(),
        "Hibernate Blob fast path must not fall back to one read() dispatch per byte"
    );
    assert_eq!(ctx.get_field_by_name(stream, "read"), Value::Int(1));
    let count_obj = match ctx.get_field_by_name(stream, "count") {
        Value::Object(Some(obj)) => obj,
        other => panic!("expected boxed Long count, got {other:?}"),
    };
    assert_eq!(ctx.get_field(count_obj, 0), Value::Long(5));
    for i in 2..10 {
        assert_eq!(ctx.get_array_element(dst, i), Value::Int(0));
    }
}

#[test]
fn hibernate_lob_stream_bulk_read_reports_eof_after_marking_read() {
    let mut ctx = MockNativeContext::new();
    let stream = make_hibernate_lob_stream(&mut ctx, 0);
    let dst = ctx.new_array(ArrayElementType::Byte, 4);

    let n = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(stream)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(4),
        ],
    )
    .unwrap();

    assert_eq!(n, Some(Value::Int(-1)));
    assert_eq!(ctx.get_field_by_name(stream, "read"), Value::Int(1));
}

#[test]
fn inputstream_super_read_bytes_uses_base_default_loop() {
    let mut ctx = MockNativeContext::new();
    let class = "org/bouncycastle/asn1/IndefiniteLengthInputStream";
    let this = ctx.alloc_object_with_class(2, class);
    ctx.declare_method(class, "read", "([BII)I");
    let dst = ctx.new_array(ArrayElementType::Byte, 4);
    ctx.script("read", "()I", Ok(Some(Value::Int(0x41))));
    ctx.script("read", "()I", Ok(Some(Value::Int(0x42))));

    let n = native_bais_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(2),
        ],
    )
    .unwrap();

    assert_eq!(n, Some(Value::Int(2)));
    assert_eq!(ctx.get_array_element(dst, 0), Value::Int(0x41));
    assert_eq!(ctx.get_array_element(dst, 1), Value::Int(0x42));
    let calls = ctx.recorded_calls();
    assert_eq!(calls.len(), 2);
    assert!(calls
        .iter()
        .all(|c| c.method_name == "read" && c.descriptor == "()I"));
}

#[test]
fn available_reflects_slot_3_count() {
    let mut ctx = MockNativeContext::new();
    let (this, _) = make_bais(&mut ctx, b"abcd");
    let av = native_bais_available(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    assert_eq!(av, Some(Value::Int(4)));
    native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    let av2 = native_bais_available(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    assert_eq!(av2, Some(Value::Int(3)));
}

#[test]
fn foreign_input_stream_does_not_probe_bais_slots() {
    let mut ctx = MockNativeContext::new();
    let text_io = ctx.alloc_object_with_class(1, "org/python/core/io/TextIOInputStream");

    let available = native_bais_available(&mut ctx, &[Value::Object(Some(text_io))]).unwrap();
    assert_eq!(available, Some(Value::Int(0)));
    let read = native_bais_read(&mut ctx, &[Value::Object(Some(text_io))]).unwrap();
    assert_eq!(read, Some(Value::Int(-1)));

    assert_eq!(ctx.field_read_count(text_io, BAIS_FIELD_DATA), 0);
    assert_eq!(ctx.field_read_count(text_io, BAIS_FIELD_POS), 0);
    assert_eq!(ctx.field_read_count(text_io, BAIS_FIELD_COUNT), 0);
}

#[test]
fn skip_advances_within_count() {
    let mut ctx = MockNativeContext::new();
    let (this, _) = make_bais(&mut ctx, b"abcdefgh");
    let s = native_bais_skip(&mut ctx, &[Value::Object(Some(this)), Value::Long(3)]).unwrap();
    assert_eq!(s, Some(Value::Long(3)));
    let a = native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    assert_eq!(a, Some(Value::Int(b'd' as i32)));
}

#[test]
fn reset_restores_to_mark() {
    let mut ctx = MockNativeContext::new();
    let (this, _) = make_bais(&mut ctx, b"abcdef");
    native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    ctx.set_field(this, BAIS_FIELD_MARK, Value::Int(2));
    native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    native_bais_reset(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    assert_eq!(ctx.get_field(this, BAIS_FIELD_POS), Value::Int(2));
    let c = native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    assert_eq!(c, Some(Value::Int(b'c' as i32)));
}

#[test]
fn init_offset_sets_pos_mark_count_correctly() {
    let mut ctx = MockNativeContext::new();
    let buf = ctx.new_array(ArrayElementType::Byte, 10);
    for i in 0..10 {
        ctx.set_array_element(buf, i, Value::Int(b'0' as i32 + i as i32));
    }
    let this = ctx.alloc_object(4);
    native_bais_init_offset(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(buf)),
            Value::Int(3),
            Value::Int(4),
        ],
    )
    .unwrap();
    assert_eq!(ctx.get_field(this, BAIS_FIELD_POS), Value::Int(3));
    assert_eq!(ctx.get_field(this, BAIS_FIELD_MARK), Value::Int(3));
    assert_eq!(ctx.get_field(this, BAIS_FIELD_COUNT), Value::Int(7));
}

// -----------------------------------------------------------------------
// The BAIS lifetime hook (G48-1) — the input-side twin of the BAOS one.
//
// The observer is a process-wide `OnceLock`, so a test that installs a
// hook installs it for EVERY test in this binary. The recorder below is
// therefore armed per THREAD: unarmed threads take a `None` branch and
// record nothing, which is what keeps this from perturbing the ~dozen
// other tests in this module that drain a stream to EOF. Installing is
// idempotent through `install_once`, because `OnceLock::set` silently
// fails on the second call and a second hook would never be seen.
// -----------------------------------------------------------------------

use cratonvm_native_api::registry::BaisEvent;
use std::cell::RefCell;

thread_local! {
    static RECORDED: RefCell<Option<Vec<(ObjectRef, BaisEvent)>>> =
        const { RefCell::new(None) };
}

fn recording_hook(
    _ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    event: BaisEvent,
) -> Result<(), MethodCallFailed> {
    RECORDED.with(|c| {
        if let Some(log) = c.borrow_mut().as_mut() {
            log.push((stream, event));
        }
    });
    Ok(())
}

/// Arm recording on THIS thread and install the process hook if no test
/// has yet. Returns the events seen while `body` ran.
fn with_recorder(body: impl FnOnce()) -> Vec<(ObjectRef, BaisEvent)> {
    cratonvm_native_api::registry::install_bais_event_hook(recording_hook);
    RECORDED.with(|c| *c.borrow_mut() = Some(Vec::new()));
    body();
    RECORDED.with(|c| c.borrow_mut().take()).unwrap_or_default()
}

#[test]
fn eof_and_close_reach_the_observer_and_do_not_change_what_they_return() {
    let events = with_recorder(|| {
        let mut ctx = MockNativeContext::new();
        let (this, _) = make_bais(&mut ctx, b"ok");

        // Two payload bytes: no EOF yet, so no event yet.
        for want in [b'o' as i32, b'k' as i32] {
            let got = native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap();
            assert_eq!(got, Some(Value::Int(want)));
        }
        assert!(
            RECORDED.with(|c| c.borrow().as_ref().unwrap().is_empty()),
            "a read that returned a byte is not an EOF and must not fire"
        );

        // The EOF read still answers -1, with an observer installed.
        let eof = native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap();
        assert_eq!(eof, Some(Value::Int(-1)), "the observer must not edit EOF");

        // close() is still a no-op returning no value.
        let closed = native_bais_close(&mut ctx, &[Value::Object(Some(this))]).unwrap();
        assert_eq!(closed, None, "BAIS.close() has no effect and no return");

        RECORDED.with(|c| {
            let log = c.borrow();
            let log = log.as_ref().unwrap();
            assert_eq!(
                log.iter().map(|(_, e)| *e).collect::<Vec<_>>(),
                vec![BaisEvent::Eof, BaisEvent::Close],
                "the drain sequence this vector performs, in order"
            );
            assert!(
                log.iter().all(|(s, _)| *s == this),
                "every event must name the stream it came from — the observer \
                 maps that object back to its carrier and has nothing else to key on"
            );
        });
    });
    assert_eq!(events.len(), 2);
}

#[test]
fn every_exhausted_read_fires_not_only_the_first() {
    // The contract `BaisEvent::Eof` states, asserted rather than trusted:
    // the transition into EOF is not observable from inside the read body
    // (a stream constructed empty is at `pos >= count` on its first read),
    // so the event fires on every exhausted read and observers MUST be
    // idempotent. A future "optimisation" that fires only once would make
    // the empty-body case silent.
    let events = with_recorder(|| {
        let mut ctx = MockNativeContext::new();
        let (empty, _) = make_bais(&mut ctx, b"");
        for _ in 0..3 {
            assert_eq!(
                native_bais_read(&mut ctx, &[Value::Object(Some(empty))]).unwrap(),
                Some(Value::Int(-1))
            );
        }
    });
    assert_eq!(
        events.iter().filter(|(_, e)| *e == BaisEvent::Eof).count(),
        3,
        "an empty stream is at EOF from its first read; all three must fire"
    );
}

#[test]
fn the_bulk_read_fires_the_same_eof_and_a_zero_length_read_does_not() {
    let events = with_recorder(|| {
        let mut ctx = MockNativeContext::new();
        let (this, _) = make_bais(&mut ctx, b"ok");
        let dst = ctx.new_array(ArrayElementType::Byte, 8);

        // A zero-length read on a stream that is NOT exhausted returns 0
        // and is not an EOF. This pins the dispatch BELOW the `pos >=
        // count` test, which is where the existing ordering comment says
        // the EOF decision has to be made.
        let zero = native_bais_read_bytes(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(dst)),
                Value::Int(0),
                Value::Int(0),
            ],
        )
        .unwrap();
        assert_eq!(zero, Some(Value::Int(0)));
        assert!(
            RECORDED.with(|c| c.borrow().as_ref().unwrap().is_empty()),
            "read(b, 0, 0) on a live stream is not an EOF"
        );

        // Drain, then hit EOF through the bulk shape.
        let n = native_bais_read_bytes(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(dst)),
                Value::Int(0),
                Value::Int(8),
            ],
        )
        .unwrap();
        assert_eq!(n, Some(Value::Int(2)));
        let eof = native_bais_read_bytes(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(dst)),
                Value::Int(0),
                Value::Int(8),
            ],
        )
        .unwrap();
        assert_eq!(eof, Some(Value::Int(-1)));
    });
    assert_eq!(
        events.iter().map(|(_, e)| *e).collect::<Vec<_>>(),
        vec![BaisEvent::Eof],
        "exactly one EOF, from the exhausted bulk read"
    );
}

#[test]
fn an_unarmed_thread_sees_the_pre_hook_behaviour_exactly() {
    // The conservatism witness. With no observer recording, every value
    // this change touches is what it was before: -1 at EOF, `None` from
    // close(), payload bytes unchanged. `native-builtins` depends on this
    // crate and nothing installs a hook until the nominated consumer
    // lands, so this is the state the whole tree is in today.
    let mut ctx = MockNativeContext::new();
    let (this, _) = make_bais(&mut ctx, b"ok");
    assert_eq!(
        native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap(),
        Some(Value::Int(b'o' as i32))
    );
    assert_eq!(
        native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap(),
        Some(Value::Int(b'k' as i32))
    );
    assert_eq!(
        native_bais_read(&mut ctx, &[Value::Object(Some(this))]).unwrap(),
        Some(Value::Int(-1))
    );
    assert_eq!(
        native_bais_close(&mut ctx, &[Value::Object(Some(this))]).unwrap(),
        None
    );
    // A null receiver still returns quietly rather than dispatching.
    assert_eq!(
        native_bais_close(&mut ctx, &[Value::Object(None)]).unwrap(),
        None
    );
}
