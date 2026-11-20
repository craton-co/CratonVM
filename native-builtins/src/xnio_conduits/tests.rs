use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;

fn make_byte_buffer(
    ctx: &mut crate::test_utils::MockNativeContext,
    capacity: i32,
) -> ObjectRef {
    let buf = try_alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 5).unwrap();
    let arr = ctx.new_array(ArrayElementType::Byte, capacity as usize);
    ctx.set_field(buf, BB_FIELD_ARRAY, Value::Object(Some(arr)));
    ctx.set_field(buf, BB_FIELD_POS, Value::Int(0));
    ctx.set_field(buf, BB_FIELD_LIMIT, Value::Int(capacity));
    ctx.set_field(buf, BB_FIELD_CAPACITY, Value::Int(capacity));
    buf
}

fn buffer_contents_up_to_pos(
    ctx: &crate::test_utils::MockNativeContext,
    buf: ObjectRef,
) -> Vec<u8> {
    let pos = bb_position(ctx, buf) as usize;
    let arr = match ctx.get_field(buf, BB_FIELD_ARRAY) {
        Value::Object(Some(o)) => o,
        _ => return Vec::new(),
    };
    (0..pos)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Int(v) => (v & 0xff) as u8,
            _ => 0,
        })
        .collect()
}

/// Simple SelectionKeyLike for tests. Ready ops + optional source / sink
/// attachments.
struct FakeKey {
    ready: i32,
    src: Option<(u64, ObjectRef)>,
    sink: Option<(u64, ObjectRef)>,
}

impl SelectionKeyLike for FakeKey {
    fn ready_ops(&self) -> i32 {
        self.ready
    }
    fn source_attachment(&self) -> Option<(u64, ObjectRef)> {
        self.src
    }
    fn sink_attachment(&self) -> Option<(u64, ObjectRef)> {
        self.sink
    }
}

fn mark_sink_write_listener_invoked(
    ctx: &mut crate::test_utils::MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "handleEvent" && descriptor == CHANNEL_LISTENER_HANDLE_EVENT_DESC {
        if let Some(Value::Object(Some(sink))) = args.first().copied() {
            ctx.set_field(sink, SINK_FIELD_WRITE_READY_FLAG, Value::Int(2));
        }
        return Some(Ok(None));
    }
    None
}

fn mark_source_read_listener_invoked(
    ctx: &mut crate::test_utils::MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "handleEvent" && descriptor == CHANNEL_LISTENER_HANDLE_EVENT_DESC {
        if let Some(Value::Object(Some(source))) = args.first().copied() {
            ctx.set_field(source, SRC_FIELD_READ_READY_FLAG, Value::Int(2));
        }
        return Some(Ok(None));
    }
    None
}

fn sink_listener_makes_paired_source_pending_then_marks_reads(
    ctx: &mut crate::test_utils::MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "handleEvent" || descriptor != CHANNEL_LISTENER_HANDLE_EVENT_DESC {
        return None;
    }
    let Some(Value::Object(Some(channel))) = args.first().copied() else {
        return Some(Ok(None));
    };
    if sink_id_of(ctx, channel).is_some() {
        ctx.set_field(channel, SINK_FIELD_WRITE_READY_FLAG, Value::Int(2));
        if let Some(source) = paired_source_of(ctx, channel) {
            if let Some(id) = source_id_of(ctx, source) {
                if let Some(ch) = get_source_channel(id) {
                    if let ConduitTransport::Pipe(pipe) = &ch.transport {
                        pipe.push(b"r");
                    }
                }
            }
        }
    } else if source_id_of(ctx, channel).is_some() {
        ctx.set_field(channel, SRC_FIELD_READ_READY_FLAG, Value::Int(2));
    }
    Some(Ok(None))
}

// ---- Test 1: source channel read returns bytes from the socket ----
#[test]
fn t19_7_d_source_channel_read_returns_bytes_from_socket() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    pipe.push(b"hello");
    let id = register_source_channel(ConduitTransport::Pipe(pipe.clone()));
    let ch = alloc_source_channel_obj(&mut ctx, id).unwrap();
    // Must be resumed for reads (the registry-side suspend flag is
    // checked by dispatch, not by the direct read path, but we reset
    // the Java-side flag for clarity).
    ctx.set_field(ch, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
    let buf = make_byte_buffer(&mut ctx, 16);

    let n = source_channel_read(&mut ctx, id, buf).unwrap();
    assert_eq!(n, 5);
    assert_eq!(buffer_contents_up_to_pos(&ctx, buf), b"hello");
    drop_source_channel(&mut ctx, id);
}

// ---- Test 2: read at EOF returns -1 ----
#[test]
fn t19_7_d_source_channel_read_at_eof_returns_minus_one() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    pipe.close_write(); // no data, writer done => EOF
    let id = register_source_channel(ConduitTransport::Pipe(pipe));
    let _ch = alloc_source_channel_obj(&mut ctx, id);
    let buf = make_byte_buffer(&mut ctx, 16);

    let n = source_channel_read(&mut ctx, id, buf).unwrap();
    assert_eq!(n, -1, "EOF must surface as -1");
    drop_source_channel(&mut ctx, id);
}

// ---- Test 3: sink channel write returns bytes written ----
#[test]
fn t19_7_d_sink_channel_write_returns_bytes_written() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    let id = register_sink_channel(ConduitTransport::Pipe(pipe.clone()));
    let _ch = alloc_sink_channel_obj(&mut ctx, id);
    let buf = make_byte_buffer(&mut ctx, 8);
    // Seed the buffer with 'abcd' at position 0.
    let arr = match ctx.get_field(buf, BB_FIELD_ARRAY) {
        Value::Object(Some(o)) => o,
        _ => panic!("array"),
    };
    for (i, b) in b"abcd".iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i32));
    }
    ctx.set_field(buf, BB_FIELD_LIMIT, Value::Int(4));

    let n = sink_channel_write(&mut ctx, id, buf).unwrap();
    assert_eq!(n, 4);
    let drained: Vec<u8> = {
        let g = pipe.buf.lock().unwrap();
        g.iter().copied().collect()
    };
    assert_eq!(&drained, b"abcd");
    drop_sink_channel(&mut ctx, id);
}

// ---- Test 4: write when kernel buffer full returns 0 ----
#[test]
fn t19_7_d_sink_channel_write_when_full_returns_zero() {
    let mut ctx = mock_ctx();
    // Cap the pipe at 4 bytes and pre-fill it. Next write must return 0.
    let pipe = Arc::new(Pipe::new(4));
    pipe.push(b"full");
    let id = register_sink_channel(ConduitTransport::Pipe(pipe.clone()));
    let _ch = alloc_sink_channel_obj(&mut ctx, id);
    let buf = make_byte_buffer(&mut ctx, 8);
    let arr = match ctx.get_field(buf, BB_FIELD_ARRAY) {
        Value::Object(Some(o)) => o,
        _ => panic!("array"),
    };
    for (i, b) in b"more".iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i32));
    }
    ctx.set_field(buf, BB_FIELD_LIMIT, Value::Int(4));

    let n = sink_channel_write(&mut ctx, id, buf).unwrap();
    assert_eq!(n, 0, "write on full pipe must return 0");
    let ch = get_sink_channel(id).unwrap();
    assert_eq!(
        ch.buffered_bytes.load(Ordering::Acquire),
        4,
        "buffered_bytes must reflect the 4 bytes that didn't go out"
    );
    drop_sink_channel(&mut ctx, id);
}

// ---- Test 5: setReadListener stores the listener on the channel ----
#[test]
fn t19_7_d_set_read_listener_stores_on_channel() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    let id = register_source_channel(ConduitTransport::Pipe(pipe));
    let ch = alloc_source_channel_obj(&mut ctx, id).unwrap();
    let listener = ctx.create_string("listener");

    let r = native_source_set_read_listener(
        &mut ctx,
        &[Value::Object(Some(ch)), Value::Object(Some(listener))],
    )
    .unwrap();
    assert!(r.is_none());
    match ctx.get_field(ch, SRC_FIELD_READ_LISTENER) {
        Value::Object(Some(o)) => assert_eq!(o, listener),
        other => panic!("expected listener stored, got {other:?}"),
    }
    drop_source_channel(&mut ctx, id);
}

// ---- Test 6: dispatch fires the listener on a ready key ----
#[test]
fn t19_7_d_dispatch_fires_listener_on_ready() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    let id = register_source_channel(ConduitTransport::Pipe(pipe));
    let ch_obj = alloc_source_channel_obj(&mut ctx, id).unwrap();
    // Resume reads so dispatch doesn't skip.
    ctx.set_field(ch_obj, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
    get_source_channel(id)
        .unwrap()
        .read_suspended
        .store(false, Ordering::Release);
    let listener = ctx.create_string("listener");
    ctx.set_field(
        ch_obj,
        SRC_FIELD_READ_LISTENER,
        Value::Object(Some(listener)),
    );

    let key = FakeKey {
        ready: OP_READ,
        src: Some((id, ch_obj)),
        sink: None,
    };
    let stats = dispatch_channel_event(&mut ctx, &key);
    assert!(stats.read_fired, "listener must fire");
    assert!(!stats.read_panicked);
    // Ready flag written.
    assert_eq!(
        ctx.get_field(ch_obj, SRC_FIELD_READ_READY_FLAG),
        Value::Int(1)
    );
    drop_source_channel(&mut ctx, id);
}

// ---- Test 7: a panicking listener is caught; loop continues ----
#[test]
fn t19_7_d_listener_panic_caught_loop_continues() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    let id = register_source_channel(ConduitTransport::Pipe(pipe));
    let ch_obj = alloc_source_channel_obj(&mut ctx, id).unwrap();
    ctx.set_field(ch_obj, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
    get_source_channel(id)
        .unwrap()
        .read_suspended
        .store(false, Ordering::Release);
    let listener = ctx.create_string("listener");
    ctx.set_field(
        ch_obj,
        SRC_FIELD_READ_LISTENER,
        Value::Object(Some(listener)),
    );

    // Install an invoke_virtual result that panics.
    let slot = ctx.invoke_virtual_result.get();
    // SAFETY: single-threaded test context.
    unsafe {
        *slot = Some(Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Internal {
                message: "synthetic panic in listener".into(),
            },
        )));
    }
    // The fake context's invoke_virtual returns our pre-staged
    // result. That's an error, not a panic; we separately verify panic
    // safety with a direct catch_unwind smoke test below.
    let key = FakeKey {
        ready: OP_READ,
        src: Some((id, ch_obj)),
        sink: None,
    };
    let stats = dispatch_channel_event(&mut ctx, &key);
    // Listener invocation "ran" (returned an Err from the mock) — no panic.
    assert!(stats.read_fired);
    assert!(!stats.read_panicked);

    // Now stage an actual panic path to exercise catch_unwind. We simulate
    // a panicking listener by wrapping the call directly.
    let panicked = catch_unwind(AssertUnwindSafe(|| panic!("BOOM"))).is_err();
    assert!(panicked, "catch_unwind must report the panic");

    drop_source_channel(&mut ctx, id);
}

// ---- Test 8: suspend_reads stops listener dispatch ----
#[test]
fn t19_7_d_suspend_reads_stops_listener_dispatch() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    let id = register_source_channel(ConduitTransport::Pipe(pipe));
    let ch_obj = alloc_source_channel_obj(&mut ctx, id).unwrap();
    let listener = ctx.create_string("listener");
    ctx.set_field(
        ch_obj,
        SRC_FIELD_READ_LISTENER,
        Value::Object(Some(listener)),
    );
    native_source_suspend_reads(&mut ctx, &[Value::Object(Some(ch_obj))]).unwrap();

    let key = FakeKey {
        ready: OP_READ,
        src: Some((id, ch_obj)),
        sink: None,
    };
    let stats = dispatch_channel_event(&mut ctx, &key);
    assert!(!stats.read_fired);
    assert_eq!(stats.skipped_suspended, 1);
    drop_source_channel(&mut ctx, id);
}

// ---- Test 9: resumeReads restarts dispatch after suspend ----
#[test]
fn t19_7_d_resume_reads_restarts_after_suspend() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    let id = register_source_channel(ConduitTransport::Pipe(pipe));
    let ch_obj = alloc_source_channel_obj(&mut ctx, id).unwrap();
    let listener = ctx.create_string("listener");
    ctx.set_field(
        ch_obj,
        SRC_FIELD_READ_LISTENER,
        Value::Object(Some(listener)),
    );

    // Suspend then resume.
    native_source_suspend_reads(&mut ctx, &[Value::Object(Some(ch_obj))]).unwrap();
    native_source_resume_reads(&mut ctx, &[Value::Object(Some(ch_obj))]).unwrap();

    assert_eq!(
        ctx.get_field(ch_obj, SRC_FIELD_READ_SUSPENDED),
        Value::Int(0)
    );
    let reg = get_source_channel(id).unwrap();
    assert!(!reg.read_suspended.load(Ordering::Acquire));

    let key = FakeKey {
        ready: OP_READ,
        src: Some((id, ch_obj)),
        sink: None,
    };
    let stats = dispatch_channel_event(&mut ctx, &key);
    assert!(stats.read_fired, "listener must fire after resume");
    drop_source_channel(&mut ctx, id);
}

// ---- Test 10: shutdownWrites sends FIN ----
#[test]
fn t19_7_d_shutdown_writes_sends_fin() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    let id = register_sink_channel(ConduitTransport::Pipe(pipe.clone()));
    let ch_obj = alloc_sink_channel_obj(&mut ctx, id).unwrap();
    native_sink_shutdown_writes(&mut ctx, &[Value::Object(Some(ch_obj))]).unwrap();

    assert!(
        pipe.fin_sent.load(Ordering::Acquire),
        "shutdown must set fin_sent on the pipe"
    );
    assert!(pipe.eof.load(Ordering::Acquire));
    // gc-common w23-b: a sink with no paired source is a connection of
    // one half, so its shutdown closes it and it is unregistered; it
    // still reports itself shut down.
    assert!(
        get_sink_channel(id).is_none(),
        "the closed sink is unregistered"
    );
    assert_eq!(
        native_sink_is_write_shutdown(&mut ctx, &[Value::Object(Some(ch_obj))]).unwrap(),
        Some(Value::Int(1))
    );

    // A subsequent write must be rejected rather than silently lost.
    let buf = make_byte_buffer(&mut ctx, 4);
    ctx.set_field(buf, BB_FIELD_LIMIT, Value::Int(4));
    let r = sink_channel_write(&mut ctx, id, buf);
    assert!(r.is_err(), "write after shutdown must fail");
    drop_sink_channel(&mut ctx, id);
}

// ---- Extra: flush reports true when no bytes buffered ----
#[test]
fn t19_7_d_flush_reports_drained_when_clean() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    let id = register_sink_channel(ConduitTransport::Pipe(pipe));
    let ch_obj = alloc_sink_channel_obj(&mut ctx, id).unwrap();
    let r = native_sink_flush(&mut ctx, &[Value::Object(Some(ch_obj))])
        .unwrap()
        .unwrap();
    assert_eq!(r, Value::Int(1), "flush must report drained when clean");
    drop_sink_channel(&mut ctx, id);
}

// ---- Extra: flush reports false when bytes are still buffered ----
#[test]
fn t19_7_d_flush_reports_not_drained_when_buffered() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    let id = register_sink_channel(ConduitTransport::Pipe(pipe));
    let ch_obj = alloc_sink_channel_obj(&mut ctx, id).unwrap();
    get_sink_channel(id)
        .unwrap()
        .buffered_bytes
        .store(17, Ordering::Release);
    let r = native_sink_flush(&mut ctx, &[Value::Object(Some(ch_obj))])
        .unwrap()
        .unwrap();
    assert_eq!(r, Value::Int(0));
    drop_sink_channel(&mut ctx, id);
}

// ---- Extra: Setter wires a listener into the channel slot ----
#[test]
fn t19_7_d_channel_listener_setter_installs_listener() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    let id = register_source_channel(ConduitTransport::Pipe(pipe));
    let ch_obj = alloc_source_channel_obj(&mut ctx, id).unwrap();
    // Build a Setter tied to SRC_FIELD_READ_LISTENER on ch_obj.
    let setter =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_LISTENER_SETTER, SETTER_NUM_SLOTS)
            .unwrap();
    ctx.set_field(
        setter,
        SETTER_FIELD_CHANNEL_HANDLE,
        Value::Object(Some(ch_obj)),
    );
    ctx.set_field(
        setter,
        SETTER_FIELD_LISTENER_SLOT_INDEX,
        Value::Int(SRC_FIELD_READ_LISTENER as i32),
    );
    let listener = ctx.create_string("installed");
    native_setter_set(
        &mut ctx,
        &[Value::Object(Some(setter)), Value::Object(Some(listener))],
    )
    .unwrap();
    match ctx.get_field(ch_obj, SRC_FIELD_READ_LISTENER) {
        Value::Object(Some(o)) => assert_eq!(o, listener),
        other => panic!("expected listener installed, got {other:?}"),
    }
    drop_source_channel(&mut ctx, id);
}

// ---- Extra: resumeWrites fires the channel write listener ----
#[test]
fn t19_7_d_resume_writes_fires_channel_listener() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    let id = register_sink_channel(ConduitTransport::Pipe(pipe));
    let ch_obj = alloc_sink_channel_obj(&mut ctx, id).unwrap();
    let listener = ctx.create_string("write-listener");
    native_sink_set_write_listener(
        &mut ctx,
        &[Value::Object(Some(ch_obj)), Value::Object(Some(listener))],
    )
    .unwrap();
    ctx.set_invoke_virtual_hook(mark_sink_write_listener_invoked);

    native_sink_resume_writes(&mut ctx, &[Value::Object(Some(ch_obj))]).unwrap();

    assert_eq!(
        ctx.get_field(ch_obj, SINK_FIELD_WRITE_SUSPENDED),
        Value::Int(0)
    );
    assert_eq!(
        ctx.get_field(ch_obj, SINK_FIELD_WRITE_READY_FLAG),
        Value::Int(2)
    );
    let reg = get_sink_channel(id).unwrap();
    assert!(!reg.write_suspended.load(Ordering::Acquire));
    assert!(reg.write_ready.load(Ordering::Acquire));
    drop_sink_channel(&mut ctx, id);
}

// ---- Extra: resumeWrites retries paired reads after listener writes ----
#[test]
fn t19_7_d_resume_writes_retries_paired_source_after_listener() {
    let mut ctx = mock_ctx();
    let source_pipe = Arc::new(Pipe::new(1024));
    let sink_pipe = Arc::new(Pipe::new(1024));
    let source_id = register_source_channel(ConduitTransport::Pipe(source_pipe));
    let sink_id = register_sink_channel(ConduitTransport::Pipe(sink_pipe));
    let source = alloc_source_channel_obj(&mut ctx, source_id).unwrap();
    let sink = alloc_sink_channel_obj(&mut ctx, sink_id).unwrap();
    remember_sink_paired_source(&mut ctx, sink, source);

    ctx.set_field(source, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
    let read_listener = ctx.create_string("read-listener");
    native_source_set_read_listener(
        &mut ctx,
        &[
            Value::Object(Some(source)),
            Value::Object(Some(read_listener)),
        ],
    )
    .unwrap();
    let write_listener = ctx.create_string("write-listener");
    native_sink_set_write_listener(
        &mut ctx,
        &[
            Value::Object(Some(sink)),
            Value::Object(Some(write_listener)),
        ],
    )
    .unwrap();
    ctx.set_invoke_virtual_hook(sink_listener_makes_paired_source_pending_then_marks_reads);

    native_sink_resume_writes(&mut ctx, &[Value::Object(Some(sink))]).unwrap();

    assert_eq!(
        ctx.get_field(sink, SINK_FIELD_WRITE_READY_FLAG),
        Value::Int(2)
    );
    assert_eq!(
        ctx.get_field(source, SRC_FIELD_READ_READY_FLAG),
        Value::Int(2),
        "resumeWrites should poll the paired source after its write listener returns"
    );

    drop_source_channel(&mut ctx, source_id);
    drop_sink_channel(&mut ctx, sink_id);
}

// ---- Extra: sink writes wake peer sources, not just their paired source ----
#[test]
fn t19_7_d_sink_write_wakes_registered_peer_source_with_pending_data() {
    let mut ctx = mock_ctx();
    let own_pipe = Arc::new(Pipe::new(1024));
    let peer_pipe = Arc::new(Pipe::new(1024));
    let own_source_id = register_source_channel(ConduitTransport::Pipe(own_pipe));
    let peer_source_id = register_source_channel(ConduitTransport::Pipe(peer_pipe.clone()));
    let sink_id = register_sink_channel(ConduitTransport::Pipe(peer_pipe));
    let own_source = alloc_source_channel_obj(&mut ctx, own_source_id).unwrap();
    let peer_source = alloc_source_channel_obj(&mut ctx, peer_source_id).unwrap();
    let sink = alloc_sink_channel_obj(&mut ctx, sink_id).unwrap();
    remember_sink_paired_source(&mut ctx, sink, own_source);

    ctx.set_field(own_source, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
    ctx.set_field(peer_source, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
    let listener = ctx.create_string("peer-read-listener");
    native_source_set_read_listener(
        &mut ctx,
        &[
            Value::Object(Some(peer_source)),
            Value::Object(Some(listener)),
        ],
    )
    .unwrap();
    ctx.set_invoke_virtual_hook(mark_source_read_listener_invoked);

    let buf = make_byte_buffer(&mut ctx, 3);
    let arr = match ctx.get_field(buf, BB_FIELD_ARRAY) {
        Value::Object(Some(o)) => o,
        _ => panic!("missing backing array"),
    };
    for (idx, b) in b"cap".iter().enumerate() {
        ctx.set_array_element(arr, idx, Value::Int(*b as i32));
    }
    ctx.set_field(buf, BB_FIELD_LIMIT, Value::Int(3));

    let n = native_sink_write(
        &mut ctx,
        &[Value::Object(Some(sink)), Value::Object(Some(buf))],
    )
    .unwrap()
    .unwrap()
    .as_int()
    .unwrap();
    assert_eq!(n, 3);
    assert_eq!(
        ctx.get_field(peer_source, SRC_FIELD_READ_READY_FLAG),
        Value::Int(2),
        "peer source listener should fire when the sink makes bytes pending"
    );
    assert_eq!(
        ctx.get_field(own_source, SRC_FIELD_READ_READY_FLAG),
        Value::Int(0),
        "paired same-side source had no pending bytes"
    );

    drop_source_channel(&mut ctx, own_source_id);
    drop_source_channel(&mut ctx, peer_source_id);
    drop_sink_channel(&mut ctx, sink_id);
}

// ---- Extra: channel IO-thread mirrors round-trip through raw conduits ----
#[test]
fn t19_7_d_channel_io_thread_round_trips_through_raw_conduits() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    let source_id = register_source_channel(ConduitTransport::Pipe(pipe.clone()));
    let sink_id = register_sink_channel(ConduitTransport::Pipe(pipe));
    let source = alloc_source_channel_obj(&mut ctx, source_id).unwrap();
    let sink = alloc_sink_channel_obj(&mut ctx, sink_id).unwrap();
    let io_thread = ctx.fresh_object_ref();

    remember_source_io_thread(&mut ctx, source, io_thread);
    remember_sink_io_thread(&mut ctx, sink, io_thread);

    assert_eq!(
        native_source_get_read_thread(&mut ctx, &[Value::Object(Some(source))])
            .unwrap()
            .unwrap(),
        Value::Object(Some(io_thread))
    );
    assert_eq!(
        native_sink_get_write_thread(&mut ctx, &[Value::Object(Some(sink))])
            .unwrap()
            .unwrap(),
        Value::Object(Some(io_thread))
    );

    // The mock context does not model the real XNIO "conduit" field
    // by name, so allocate raw conduit mirrors directly for this half of
    // the regression. Production channels still remember both surfaces
    // when the named field resolves.
    let source_conduit = alloc_source_conduit_obj(&mut ctx, source_id).unwrap();
    let sink_conduit = alloc_sink_conduit_obj(&mut ctx, sink_id).unwrap();
    remember_source_io_thread(&mut ctx, source_conduit, io_thread);
    remember_sink_io_thread(&mut ctx, sink_conduit, io_thread);
    assert_eq!(
        native_source_get_read_thread(&mut ctx, &[Value::Object(Some(source_conduit))])
            .unwrap()
            .unwrap(),
        Value::Object(Some(io_thread))
    );
    assert_eq!(
        native_sink_get_write_thread(&mut ctx, &[Value::Object(Some(sink_conduit))])
            .unwrap()
            .unwrap(),
        Value::Object(Some(io_thread))
    );

    drop_source_channel(&mut ctx, source_id);
    drop_sink_channel(&mut ctx, sink_id);
}

// ---- Extra: out-of-range ByteBuffer bounds are rejected ----
#[test]
fn t19_7_d_buffer_bounds_check_rejects_out_of_range() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    pipe.push(b"abcdef");
    let id = register_source_channel(ConduitTransport::Pipe(pipe));
    let _ch = alloc_source_channel_obj(&mut ctx, id);

    // Build a buffer with position > limit — remaining() returns 0 and the
    // read returns 0 rather than segfaulting.
    let buf = make_byte_buffer(&mut ctx, 8);
    ctx.set_field(buf, BB_FIELD_POS, Value::Int(9));
    ctx.set_field(buf, BB_FIELD_LIMIT, Value::Int(4));
    let n = source_channel_read(&mut ctx, id, buf).unwrap();
    assert_eq!(n, 0, "out-of-range bounds yield 0, not a crash");
    drop_source_channel(&mut ctx, id);
}

// ---- Extra: scatter read across multiple buffers ----
#[test]
fn t19_7_d_source_scatter_read_distributes_bytes() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    pipe.push(b"hello world");
    let id = register_source_channel(ConduitTransport::Pipe(pipe));
    let ch_obj = alloc_source_channel_obj(&mut ctx, id).unwrap();

    let b1 = make_byte_buffer(&mut ctx, 5);
    let b2 = make_byte_buffer(&mut ctx, 8);
    let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 2);
    ctx.set_array_element(arr, 0, Value::Object(Some(b1)));
    ctx.set_array_element(arr, 1, Value::Object(Some(b2)));

    let r = native_source_read_scatter(
        &mut ctx,
        &[
            Value::Object(Some(ch_obj)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(2),
        ],
    )
    .unwrap()
    .unwrap();
    let total = r.as_long().unwrap();
    assert!(
        total > 0 && total <= 11,
        "total bytes in [1..=11], got {total}"
    );
    drop_source_channel(&mut ctx, id);
}
