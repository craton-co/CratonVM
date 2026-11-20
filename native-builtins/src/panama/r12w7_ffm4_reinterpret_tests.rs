//! Round 12 wave 7 (lane ffm4): `reinterpret(long, Arena, Consumer)` /
//! `reinterpret(Arena, Consumer)` (they did not exist: `NoSuchMethodError`),
//! and the checks every `reinterpret` overload now shares.
use super::*;
use crate::phases_late::foreign_ffm::{p67_memory_session, p67_session_slots};
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// What the hook answers for `arena.scope()`.
    static SCOPE: Cell<Option<ObjectRef>> = const { Cell::new(None) };
    /// `accept` calls the hook saw.
    static ACCEPTS: Cell<usize> = const { Cell::new(0) };
}

fn hook(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    match method {
        "scope" => Some(Ok(Some(Value::Object(SCOPE.with(|c| c.get()))))),
        "accept" => {
            ACCEPTS.with(|c| c.set(c.get() + 1));
            Some(Ok(None))
        }
        _ => None,
    }
}

fn obj(o: ObjectRef) -> Value {
    Value::Object(Some(o))
}

fn setup() -> (MockNativeContext, ObjectRef, ObjectRef, i64) {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(hook);
    ACCEPTS.with(|c| c.set(0));
    let session = match p67_memory_session(&mut ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    };
    SCOPE.with(|c| c.set(Some(session)));
    let (_id, ptr) = ctx
        .allocate_native_memory(16, 8)
        .unwrap_or_else(|| panic!("mock allocation failed"));
    let seg = match alloc_segment_carrier(&mut ctx, 6) {
        Ok(seg) => seg,
        Err(err) => panic!("no carrier: {err:?}"),
    };
    ctx.set_field(seg, 0, Value::Long(ptr as i64));
    ctx.set_field(seg, 1, Value::Long(0));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    (ctx, session, seg, ptr as i64)
}

/// The re-scoped segment has the address, the new size and the arena's
/// scope, and the cleanup is on that scope's close list with a
/// global-scope segment over the same memory -- not yet run.
#[test]
fn reinterpret_with_a_cleanup_rescopes_and_registers_it() {
    let (mut ctx, session, seg, ptr) = setup();
    let arena = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let cleanup = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let args = [obj(seg), Value::Long(16), obj(arena), obj(cleanup)];
    let out = match pe_segment_reinterpret_scoped(&mut ctx, &args, true) {
        Ok(Some(Value::Object(Some(out)))) => out,
        other => panic!("reinterpret gave {other:?}"),
    };
    assert_eq!(ctx.get_field(out, 0), Value::Long(ptr));
    assert_eq!(ctx.get_field(out, 1), Value::Long(16));
    assert_eq!(ctx.get_field(out, 2), obj(session));
    assert_eq!(ACCEPTS.with(|c| c.get()), 0);

    // Either list shape (round 13 wave 12: a linked stack by default).
    let actions = crate::phases_late::foreign_ffm::p67_session_actions(&ctx, session);
    assert_eq!(actions.len(), 1, "no close action registered");
    let record = actions[0];
    assert_eq!(ctx.get_array_element(record, 0), obj(cleanup));
    let Value::Object(Some(cleanup_seg)) = ctx.get_array_element(record, 1) else {
        panic!("no cleanup segment");
    };
    assert_eq!(ctx.get_field(cleanup_seg, 0), Value::Long(ptr));
    assert_eq!(ctx.get_field(cleanup_seg, 1), Value::Long(16));
    assert_eq!(ctx.get_field(cleanup_seg, 2), Value::Object(None));
}

/// The two-argument overload keeps the segment's own size; a null cleanup
/// registers nothing.
#[test]
fn the_unsized_overload_keeps_the_size_and_a_null_cleanup_registers_nothing() {
    let (mut ctx, session, seg, _ptr) = setup();
    ctx.set_field(seg, 1, Value::Long(12));
    let arena = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let args = [obj(seg), obj(arena), Value::Object(None)];
    let out = match pe_segment_reinterpret_scoped(&mut ctx, &args, false) {
        Ok(Some(Value::Object(Some(out)))) => out,
        other => panic!("reinterpret gave {other:?}"),
    };
    assert_eq!(ctx.get_field(out, 1), Value::Long(12));
    assert_eq!(ctx.get_field(out, 2), obj(session));
    let slots = p67_session_slots(&ctx, session);
    assert_eq!(ctx.get_field(session, slots.actions), Value::Object(None));
}

/// A closed arena: the call throws and the cleanup is not run (the JDK
/// registers through `addCloseAction`, i.e. `addInternal`).
#[test]
fn a_closed_arena_throws_without_running_the_cleanup() {
    let (mut ctx, session, seg, _ptr) = setup();
    let slots = p67_session_slots(&ctx, session);
    ctx.set_field(session, slots.state, Value::Int(0));
    let arena = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let cleanup = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let args = [obj(seg), Value::Long(16), obj(arena), obj(cleanup)];
    let err = match pe_segment_reinterpret_scoped(&mut ctx, &args, true) {
        Ok(v) => panic!("a closed arena accepted a reinterpret: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
    assert_eq!(ACCEPTS.with(|c| c.get()), 0);
    let slots = p67_session_slots(&ctx, session);
    assert_eq!(ctx.get_field(session, slots.actions), Value::Object(None));
}

/// `requireNonNull(arena)`, a negative size, and a heap segment.
#[test]
fn a_null_arena_a_negative_size_and_a_heap_segment_are_refused() {
    let (mut ctx, _session, seg, _ptr) = setup();
    let arena = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let null_arena = [
        obj(seg),
        Value::Long(16),
        Value::Object(None),
        Value::Object(None),
    ];
    let err = match pe_segment_reinterpret_scoped(&mut ctx, &null_arena, true) {
        Ok(v) => panic!("a null arena was accepted: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("NullPointer"), "{err:?}");

    let negative = [obj(seg), Value::Long(-1), obj(arena), Value::Object(None)];
    let err = match pe_segment_reinterpret_scoped(&mut ctx, &negative, true) {
        Ok(v) => panic!("a negative size was accepted: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("negative"), "{err:?}");

    let array = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
    let heap = match pe_of_array_alias(&mut ctx, &[obj(array)]) {
        Ok(Some(Value::Object(Some(heap)))) => heap,
        other => panic!("ofArray gave {other:?}"),
    };
    let err = match pe_segment_reinterpret(&mut ctx, &[obj(heap), Value::Long(8)]) {
        Ok(v) => panic!("a heap segment was reinterpreted: {v:?}"),
        Err(err) => err,
    };
    assert!(
        format!("{err:?}").contains("Not a native segment"),
        "{err:?}"
    );
}

/// The one-argument overload ends in `checkValidState()`: a closed
/// segment cannot be reinterpreted.
#[test]
fn reinterpreting_a_closed_segment_is_already_closed() {
    let (mut ctx, session, seg, _ptr) = setup();
    ctx.set_field(seg, 1, Value::Long(16));
    ctx.set_field(seg, PE_SEGMENT_ARENA_FIELD, obj(session));
    assert!(pe_segment_reinterpret(&mut ctx, &[obj(seg), Value::Long(8)]).is_ok());
    let slots = p67_session_slots(&ctx, session);
    ctx.set_field(session, slots.state, Value::Int(0));
    let err = match pe_segment_reinterpret(&mut ctx, &[obj(seg), Value::Long(8)]) {
        Ok(v) => panic!("a closed segment was reinterpreted: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");    }