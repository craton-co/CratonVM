use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::{Cell, RefCell};

thread_local! {
    /// (old, new) addresses the hook moves at the first `intValue()`.
    static W13F_MOVES: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
    /// The record `Filter.isLoggable` received.
    static W13F_SEEN: Cell<usize> = const { Cell::new(0) };
}

/// A class of the test's own (so no mock slot table claims its names)
/// whose listed instance fields resolve by name at slots 0, 1, 2, ...
fn w13f_class(
    ctx: &mut MockNativeContext,
    name: &str,
    fields: &[&str],
) -> cratonvm_types::ClassId {
    let cid = ctx.ensure_class_initialized(name).unwrap();
    let declared = fields
        .iter()
        .enumerate()
        .map(|(slot, field)| cratonvm_native_api::FieldMetadata {
            name: (*field).to_string(),
            descriptor: "Ljava/lang/Object;".to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: cid,
            is_static: false,
        })
        .collect();
    ctx.set_declared_fields(cid, declared);
    cid
}

fn w13f_move_on_int_value(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match method_name {
        "intValue" => {
            let moves = W13F_MOVES.with(|m| std::mem::take(&mut *m.borrow_mut()));
            for (old, new) in moves {
                ctx.remap_native_pin_addr_for_test(old, new);
            }
            Some(Ok(Some(Value::Int(800))))
        }
        "isLoggable" => {
            if let Some(Value::Object(Some(record))) = args.first() {
                W13F_SEEN.with(|c| c.set(record.as_ptr() as usize));
            }
            Some(Ok(Some(Value::Int(1))))
        }
        _ => None,
    }
}

/// `Handler.isLoggable(LogRecord)`: the handler and the record are moved
/// while the handler level's `intValue()` runs. The filter must be read
/// off the MOVED handler (the vacated copy has none) and handed the MOVED
/// record.
#[test]
fn w13f_handler_is_loggable_filters_the_record_moved_by_int_value() {
    let mut registry = NativeMethodRegistry::new();
    register_annotation_overrides(&mut registry);
    let is_loggable = registry
        .find(
            "java/util/logging/Handler",
            "isLoggable",
            "(Ljava/util/logging/LogRecord;)Z",
        )
        .expect("Handler.isLoggable should be registered");

    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0xF13_0101);
    let handler_cid = w13f_class(&mut ctx, "w13f/Handler", &["logLevel", "filter"]);
    let record_cid = w13f_class(&mut ctx, "w13f/Record", &["level"]);
    let handler_level = ctx.fresh_object_ref();
    let record_level = ctx.fresh_object_ref();
    let filter = ctx.fresh_object_ref();

    let this = ctx.alloc_object(handler_cid, 2);
    let moved_this = ctx.alloc_object(handler_cid, 2);
    ctx.set_field(this, 0, Value::Object(Some(handler_level)));
    ctx.set_field(moved_this, 0, Value::Object(Some(handler_level)));
    // The vacated copy no longer carries the filter; the moved one does.
    ctx.set_field(this, 1, Value::Object(None));
    ctx.set_field(moved_this, 1, Value::Object(Some(filter)));

    let record = ctx.alloc_object(record_cid, 1);
    let moved_record = ctx.alloc_object(record_cid, 1);
    ctx.set_field(record, 0, Value::Object(Some(record_level)));
    ctx.set_field(moved_record, 0, Value::Object(Some(record_level)));

    W13F_MOVES.with(|m| {
        *m.borrow_mut() = vec![
            (this.as_ptr() as usize, moved_this.as_ptr() as usize),
            (record.as_ptr() as usize, moved_record.as_ptr() as usize),
        ]
    });
    W13F_SEEN.with(|c| c.set(0));
    ctx.set_invoke_virtual_hook(w13f_move_on_int_value);

    let result = is_loggable(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(record))],
    )
    .unwrap();

    assert_eq!(result, Some(Value::Int(1)));
    assert_eq!(
        W13F_SEEN.with(Cell::get),
        moved_record.as_ptr() as usize,
        "the filter must be read off the moved handler and given the moved record"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
