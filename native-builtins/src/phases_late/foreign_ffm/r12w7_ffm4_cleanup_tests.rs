//! Round 12 wave 7 (lane ffm4): `reinterpret(.., Arena, Consumer)`'s close
//! action, the pinned close-walk failure, and the real-session fast check.
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// `accept` calls the hook saw.
    static ACCEPTS: Cell<usize> = const { Cell::new(0) };
    /// The block record whose count the hook reads at `accept` time.
    static RECORD: Cell<Option<ObjectRef>> = const { Cell::new(None) };
    /// The record's count when the cleanup ran.
    static COUNT_AT_ACCEPT: Cell<i64> = const { Cell::new(-1) };
    /// A throwable `accept` answers with, when set.
    static THROW: Cell<Option<ObjectRef>> = const { Cell::new(None) };
}

fn reset() {
    ACCEPTS.with(|c| c.set(0));
    RECORD.with(|c| c.set(None));
    COUNT_AT_ACCEPT.with(|c| c.set(-1));
    THROW.with(|c| c.set(None));
}

fn cleanup_hook(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method != "accept" {
        return None;
    }
    ACCEPTS.with(|c| c.set(c.get() + 1));
    if let Some(record) = RECORD.with(|c| c.get()) {
        if let Value::Long(n) = ctx.get_array_element(record, 0) {
            COUNT_AT_ACCEPT.with(|c| c.set(n));
        }
    }
    match THROW.with(|c| c.get()) {
        Some(thrown) => Some(Err(MethodCallFailed::ExceptionThrown(thrown))),
        None => Some(Ok(None)),
    }
}

fn session(ctx: &mut dyn NativeContext) -> ObjectRef {
    match p67_memory_session(ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    }
}

fn block_record(ctx: &dyn NativeContext, session: ObjectRef) -> ObjectRef {
    p67_session_actions(ctx, session)
        .into_iter()
        .find(|&o| p67_is_block_record(ctx, o))
        .unwrap_or_else(|| panic!("no block record"))
}

/// The cleanup registered after the arena's first allocation runs at
/// close while that allocation's block is still recorded (not yet freed),
/// and exactly once.
#[test]
fn a_cleanup_runs_at_close_before_the_blocks_are_freed() {
    reset();
    let mut mock = mock_ctx();
    mock.set_invoke_virtual_hook(cleanup_hook);
    let ctx: &mut dyn NativeContext = &mut mock;
    let mut session = session(ctx);
    let (id, _ptr) = ctx
        .allocate_native_memory(16, 8)
        .unwrap_or_else(|| panic!("mock allocation failed"));
    assert!(p67_session_record_block(ctx, session, id));
    let record = block_record(ctx, session);
    RECORD.with(|c| c.set(Some(record)));

    let cleanup = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let cleanup_seg = ctx.alloc_object(cratonvm_types::ClassId::new(0), 6);
    assert!(p67_session_add_cleanup(ctx, &mut session, cleanup, cleanup_seg).is_ok());
    assert_eq!(
        ACCEPTS.with(|c| c.get()),
        0,
        "registration must not run the cleanup"
    );

    assert!(p67_session_run_close_actions(ctx, session).is_ok());
    assert_eq!(ACCEPTS.with(|c| c.get()), 1);
    assert_eq!(
        COUNT_AT_ACCEPT.with(|c| c.get()),
        1,
        "the cleanup must run while the block is still recorded, i.e. before the free"
    );
    assert_eq!(
        ctx.get_array_element(record, 0),
        Value::Long(0),
        "then the block is freed"
    );
}

/// `addCloseAction` is `addInternal`: a closed session refuses the
/// registration with its `Already closed` and the cleanup does not run.
#[test]
fn a_closed_session_refuses_the_cleanup_without_running_it() {
    reset();
    let mut mock = mock_ctx();
    mock.set_invoke_virtual_hook(cleanup_hook);
    let ctx: &mut dyn NativeContext = &mut mock;
    let mut session = session(ctx);
    let slots = p67_session_slots(ctx, session);
    ctx.set_field(session, slots.state, Value::Int(0));
    let cleanup = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let cleanup_seg = ctx.alloc_object(cratonvm_types::ClassId::new(0), 6);
    let err = match p67_session_add_cleanup(ctx, &mut session, cleanup, cleanup_seg) {
        Ok(()) => panic!("a closed session accepted a cleanup"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
    assert_eq!(ACCEPTS.with(|c| c.get()), 0);
    assert_eq!(ctx.get_field(session, slots.actions), Value::Object(None));
}

/// A cleanup that throws does not stop the walk, its throwable is what the
/// close rethrows, and the walk leaves no pin behind.
#[test]
fn a_throwing_cleanup_is_rethrown_after_the_walk_with_no_pin_left() {
    reset();
    let mut mock = mock_ctx();
    mock.set_invoke_virtual_hook(cleanup_hook);
    let pins_before = mock.native_pin_count_for_test();
    let ctx: &mut dyn NativeContext = &mut mock;
    let thrown = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let mut session = session(ctx);
    for _ in 0..2 {
        let cleanup = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
        let cleanup_seg = ctx.alloc_object(cratonvm_types::ClassId::new(0), 6);
        assert!(p67_session_add_cleanup(ctx, &mut session, cleanup, cleanup_seg).is_ok());
    }
    THROW.with(|c| c.set(Some(thrown)));
    match p67_session_run_close_actions(ctx, session) {
        Err(MethodCallFailed::ExceptionThrown(got)) => assert_eq!(got, thrown),
        other => panic!("expected the cleanup's throwable, got {other:?}"),
    }
    assert_eq!(ACCEPTS.with(|c| c.get()), 2, "both cleanups ran");
    assert_eq!(mock.native_pin_count_for_test(), pins_before);
}

/// Only a two-element reference array is a cleanup record.
#[test]
fn only_a_two_element_reference_array_is_a_cleanup_record() {
    let mut mock = mock_ctx();
    let ctx: &mut dyn NativeContext = &mut mock;
    let a = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let pair = ctx.new_array(ArrayElementType::Reference, 2);
    ctx.set_array_element(pair, 0, Value::Object(Some(a)));
    ctx.set_array_element(pair, 1, Value::Object(Some(a)));
    let triple = ctx.new_array(ArrayElementType::Reference, 3);
    let longs = ctx.new_array(ArrayElementType::Long, 2);
    assert_eq!(p67_cleanup_record(ctx, pair), Some((a, a)));
    assert_eq!(p67_cleanup_record(ctx, triple), None);
    assert_eq!(p67_cleanup_record(ctx, longs), None);
    assert_eq!(p67_cleanup_record(ctx, a), None);
}

/// The real-session fast check passes only an open session with no owner
/// (or this thread as owner); a closed one or another owner goes to the
/// bytecode, which raises the exact exception.
#[test]
fn the_real_session_fast_check_passes_only_what_the_bytecode_would() {
    let mut mock = mock_ctx();
    let cid = match mock.ensure_class_initialized("jdk/internal/foreign/SharedSession") {
        Ok(cid) => cid,
        Err(err) => panic!("no class: {err:?}"),
    };
    let field = |name: &str, desc: &str, slot: usize| cratonvm_native_api::FieldMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: cid,
        is_static: false,
    };
    mock.set_declared_fields(
        cid,
        vec![
            field("owner", "Ljava/lang/Thread;", 0),
            field("state", "I", 1),
        ],
    );
    let ctx: &mut dyn NativeContext = &mut mock;
    let session = ctx.alloc_object(cid, 2);
    assert!(p67_session_is_real(ctx, session));

    ctx.set_field(session, 0, Value::Object(None));
    ctx.set_field(session, 1, Value::Int(0));
    assert!(
        p67_real_session_passes_natively(ctx, session),
        "open, unconfined"
    );
    ctx.set_field(session, 1, Value::Int(1));
    assert!(
        p67_real_session_passes_natively(ctx, session),
        "non-closeable"
    );
    ctx.set_field(session, 1, Value::Int(-1));
    assert!(!p67_real_session_passes_natively(ctx, session), "closed");
    let other = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    ctx.set_field(session, 1, Value::Int(0));
    ctx.set_field(session, 0, Value::Object(Some(other)));
    assert!(
        !p67_real_session_passes_natively(ctx, session),
        "another owner"
    );
}
