use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
use cratonvm_native_api::{FieldMetadata, NativeClassAccess, NativeHeapAccess};
use cratonvm_types::error::MethodCallFailed;
use std::cell::Cell;

const MESSAGE_SLOT: usize = 1;
const CAUSE_SLOT: usize = 2;

thread_local! {
    static SEEN_MESSAGE: Cell<Option<Value>> = const { Cell::new(None) };
    static SEEN_CAUSE: Cell<Option<Value>> = const { Cell::new(None) };
    static OVERRIDE_THROWS: Cell<Option<ObjectRef>> = const { Cell::new(None) };
    static OVERRIDE_SETS_CAUSE: Cell<Option<ObjectRef>> = const { Cell::new(None) };
}

fn throwable_fields(declaring: ClassId) -> Vec<FieldMetadata> {
    let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: declaring,
        is_static: false,
    };
    vec![
        field("detailMessage", "Ljava/lang/String;", MESSAGE_SLOT),
        field("cause", "Ljava/lang/Throwable;", CAUSE_SLOT),
    ]
}

/// The override: records what it sees, may call `initCause`, may throw.
fn fill_override(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if name != "fillInStackTrace" {
        return None;
    }
    SEEN_MESSAGE.with(|c| c.set(Some(ctx.get_field(receiver, MESSAGE_SLOT))));
    SEEN_CAUSE.with(|c| c.set(Some(ctx.get_field(receiver, CAUSE_SLOT))));
    if let Some(cause) = OVERRIDE_SETS_CAUSE.with(Cell::get) {
        ctx.set_field(receiver, CAUSE_SLOT, Value::Object(Some(cause)));
    }
    Some(match OVERRIDE_THROWS.with(Cell::get) {
        Some(exc) => Err(MethodCallFailed::ExceptionThrown(exc)),
        None => Ok(Some(Value::Object(Some(receiver)))),
    })
}

/// A throwable as a shadow leaves it just before the fill: message and
/// cause already assigned.
fn shadowed_throwable(ctx: &mut MockNativeContext) -> ObjectRef {
    SEEN_MESSAGE.with(|c| c.set(None));
    SEEN_CAUSE.with(|c| c.set(None));
    OVERRIDE_THROWS.with(|c| c.set(None));
    OVERRIDE_SETS_CAUSE.with(|c| c.set(None));
    let throwable = ctx
        .ensure_class_initialized("java/lang/Throwable")
        .expect("mock class");
    ctx.set_declared_fields(throwable, throwable_fields(throwable));
    let this = match ctx.new_object("test/QuietOverride") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("expected an object, got {other:?}"),
    };
    let own = ctx.class_id_of_object(this);
    ctx.set_declared_fields(own, throwable_fields(throwable));
    ctx.set_invoke_virtual_hook(fill_override);
    this
}

#[test]
fn the_override_runs_before_the_message_and_cause_are_assigned() {
    let mut ctx = mock_ctx();
    let this = shadowed_throwable(&mut ctx);
    let message = ctx.create_string("msg");
    let cause = match ctx.new_object("java/lang/Error") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("expected an object, got {other:?}"),
    };
    write_throwable_detail_message(&mut ctx, this, Value::Object(Some(message)));
    write_throwable_cause(&mut ctx, this, Value::Object(Some(cause)));

    let mut refreshed = this;
    run_fill_override_in_ctor_order(&mut ctx, &mut refreshed).expect("override returned");

    // `Throwable(String, Throwable)`: fillInStackTrace() sees the field
    // initialisers only -- detailMessage null, cause == this.
    assert_eq!(SEEN_MESSAGE.with(Cell::get), Some(Value::Object(None)));
    assert_eq!(SEEN_CAUSE.with(Cell::get), Some(Value::Object(Some(this))));
    // ... and the constructor's assignments land afterwards.
    assert_eq!(
        ctx.get_field(refreshed, MESSAGE_SLOT),
        Value::Object(Some(message))
    );
    assert_eq!(
        ctx.get_field(refreshed, CAUSE_SLOT),
        Value::Object(Some(cause))
    );
}

#[test]
fn what_the_override_throws_is_returned_for_new_to_throw() {
    let mut ctx = mock_ctx();
    let this = shadowed_throwable(&mut ctx);
    let message = ctx.create_string("msg");
    write_throwable_detail_message(&mut ctx, this, Value::Object(Some(message)));
    write_throwable_cause(&mut ctx, this, Value::Object(Some(this)));
    let thrown = match ctx.new_object("java/lang/IllegalStateException") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("expected an object, got {other:?}"),
    };
    OVERRIDE_THROWS.with(|c| c.set(Some(thrown)));

    let mut refreshed = this;
    let outcome = run_fill_override_in_ctor_order(&mut ctx, &mut refreshed);

    // Wave 7 dropped this (`let _ = invoke_virtual(..)`), so `new` completed.
    assert!(
        matches!(outcome, Err(MethodCallFailed::ExceptionThrown(e)) if e == thrown),
        "the override's exception must leave the constructor",
    );
}

#[test]
fn a_cause_left_at_the_sentinel_keeps_what_the_override_set() {
    // `Throwable(String)` never assigns `cause` after the fill, so an
    // `initCause` inside the override survives on HotSpot.
    let mut ctx = mock_ctx();
    let this = shadowed_throwable(&mut ctx);
    let message = ctx.create_string("msg");
    write_throwable_detail_message(&mut ctx, this, Value::Object(Some(message)));
    write_throwable_cause(&mut ctx, this, Value::Object(Some(this)));
    let set_by_override = match ctx.new_object("java/lang/Error") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("expected an object, got {other:?}"),
    };
    OVERRIDE_SETS_CAUSE.with(|c| c.set(Some(set_by_override)));

    let mut refreshed = this;
    run_fill_override_in_ctor_order(&mut ctx, &mut refreshed).expect("override returned");

    assert_eq!(SEEN_MESSAGE.with(Cell::get), Some(Value::Object(None)));
    assert_eq!(
        ctx.get_field(refreshed, CAUSE_SLOT),
        Value::Object(Some(set_by_override))
    );
    assert_eq!(
        ctx.get_field(refreshed, MESSAGE_SLOT),
        Value::Object(Some(message))
    );
}
