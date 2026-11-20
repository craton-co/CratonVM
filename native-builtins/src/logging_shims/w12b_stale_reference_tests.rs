use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

thread_local! {
    /// (old, new) address of the object the hook moves.
    static W12B_MOVE: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    /// The receiver / argument the hook observed at the call under test.
    static W12B_SEEN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn w12b_field(
    cid: cratonvm_types::ClassId,
    name: &str,
    slot_index: usize,
    is_static: bool,
) -> cratonvm_native_api::FieldMetadata {
    cratonvm_native_api::FieldMetadata {
        name: name.to_string(),
        descriptor: "Ljava/lang/Object;".to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: cid,
        is_static,
    }
}

/// A class of the test's own (so no mock slot table claims its names)
/// whose listed instance fields resolve by name at slots 0, 1, 2, ...
fn w12b_stream_class(
    ctx: &mut MockNativeContext,
    name: &str,
    fields: &[&str],
) -> cratonvm_types::ClassId {
    let cid = ctx.ensure_class_initialized(name).unwrap();
    let declared = fields
        .iter()
        .enumerate()
        .map(|(slot, field)| w12b_field(cid, field, slot, false))
        .collect();
    ctx.set_declared_fields(cid, declared);
    cid
}

fn w12b_move_on_close(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    match method_name {
        "flush" => Some(Ok(None)),
        "close" => {
            let (old, new) = W12B_MOVE.with(std::cell::Cell::get);
            ctx.remap_native_pin_addr_for_test(old, new);
            W12B_SEEN.with(|c| c.set(receiver.as_ptr() as usize));
            Some(Ok(None))
        }
        _ => None,
    }
}

/// `PrintStream.close()`: the stream is moved while its sink's `close()`
/// runs; `out = null` must land on the moved stream, not the old address.
#[test]
fn w12b_printstream_close_nulls_out_on_the_receiver_moved_by_close() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0xB12_0101);
    let cid = w12b_stream_class(&mut ctx, "w12b/CloseShape", &["out", "closing", "trouble"]);
    let this = ctx.alloc_object(cid, 4);
    let moved = ctx.alloc_object(cid, 4);
    let sink = ctx.fresh_object_ref();
    ctx.set_field(this, 0, Value::Object(Some(sink)));
    W12B_MOVE.with(|c| c.set((this.as_ptr() as usize, moved.as_ptr() as usize)));
    W12B_SEEN.with(|c| c.set(0));
    ctx.set_invoke_virtual_hook(w12b_move_on_close);

    native_printstream_close(&mut ctx, &[Value::Object(Some(this))]).unwrap();

    assert_eq!(
        W12B_SEEN.with(std::cell::Cell::get),
        sink.as_ptr() as usize,
        "the sink must have been closed"
    );
    assert_eq!(
        ctx.get_field(moved, 0),
        Value::Object(None),
        "`out = null` must be written on the stream at its post-close() address"
    );
    assert_eq!(
        ctx.get_field(this, 0),
        Value::Object(Some(sink)),
        "the stale pre-close() address must not be written"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

fn w12b_move_record_on_get(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match method_name {
        "size" => Some(Ok(Some(Value::Int(1)))),
        "get" => {
            // Move the LogRecord, wherever the native rooted it.
            for i in 0..ctx.native_pin_count_for_test() {
                let pinned = ctx.read_native_pin(i, receiver);
                let class = ctx.class_name_of_id(ctx.class_id_of_object(pinned));
                if class.as_deref() == Some("java/util/logging/LogRecord") {
                    let moved = ctx.fresh_object_ref();
                    let old = pinned.as_ptr() as usize;
                    ctx.remap_native_pin_addr_for_test(old, moved.as_ptr() as usize);
                    W12B_MOVE.with(|c| c.set((old, moved.as_ptr() as usize)));
                    break;
                }
            }
            let handler = ctx.fresh_object_ref();
            Some(Ok(Some(Value::Object(Some(handler)))))
        }
        "publish" => {
            if let Some(Value::Object(Some(record))) = args.first() {
                W12B_SEEN.with(|c| c.set(record.as_ptr() as usize));                }
            Some(Ok(None))
        }
        _ => None,
    }
}

/// JUL `Logger.info(String)` fan-out: the record is moved while the
/// handler list's `get(int)` runs; `publish` must receive the moved record.
#[test]
fn w12b_jul_log_msg_publishes_the_record_moved_by_get() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0xB12_0102);
    let logger = ctx.fresh_object_ref();
    let list = ctx.fresh_object_ref();
    jul_logger_handlers_set(&mut ctx, logger, list);
    let level_cid = ctx
        .ensure_class_initialized("java/util/logging/Level")
        .unwrap();
    ctx.set_declared_fields(level_cid, vec![w12b_field(level_cid, "INFO", 0, true)]);
    let message = ctx.create_string("w12b message");
    W12B_MOVE.with(|c| c.set((0, 0)));
    W12B_SEEN.with(|c| c.set(0));
    ctx.set_invoke_virtual_hook(w12b_move_record_on_get);

    jul_log_msg(
        &mut ctx,
        &[Value::Object(Some(logger)), Value::Object(Some(message))],
    )
    .unwrap();

    let (old, new) = W12B_MOVE.with(std::cell::Cell::get);
    assert_ne!(old, 0, "the record must have been rooted across `get`");
    assert_eq!(
        W12B_SEEN.with(std::cell::Cell::get),
        new,
        "`publish` must receive the record at its post-get() address"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

fn w12b_move_stream_on_to_string(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "toString" {
        return None;
    }
    let (old, new) = W12B_MOVE.with(std::cell::Cell::get);
    ctx.remap_native_pin_addr_for_test(old, new);
    let text = ctx.create_string("w12b");
    Some(Ok(Some(Value::Object(Some(text)))))
}

/// `PrintStream.print(Object)`: the stream is moved while the argument's
/// `toString()` runs. With no sink and no descriptor the write lands on
/// `ensureOpen()`'s `trouble = true`, which must be set on the MOVED
/// stream — i.e. `stream_write` was handed the live receiver.
#[test]
fn w12b_print_object_writes_through_the_receiver_moved_by_to_string() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0xB12_0103);
    // Slot 0 stays a non-reference, so `stream_fd`'s wrapper walk stops.
    let cid = w12b_stream_class(&mut ctx, "w12b/PrintShape", &["w12bSlot0", "trouble"]);
    let this = ctx.alloc_object(cid, 4);
    let moved = ctx.alloc_object(cid, 4);
    let arg = ctx.fresh_object_ref();
    W12B_MOVE.with(|c| c.set((this.as_ptr() as usize, moved.as_ptr() as usize)));
    ctx.set_invoke_virtual_hook(w12b_move_stream_on_to_string);

    native_print_object(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(arg))],
    )
    .unwrap();

    assert_eq!(
        ctx.get_field(moved, 1),
        Value::Int(1),
        "`trouble` must be recorded on the stream at its post-toString() address"
    );
    assert_eq!(
        ctx.get_field(this, 1),
        Value::Int(0),
        "the stale pre-toString() address must not be written"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
