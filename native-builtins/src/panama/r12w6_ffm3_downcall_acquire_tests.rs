//! Round 12 wave 6 (lane ffm3): a downcall acquires each argument
//! segment's session for the call and releases it afterwards; a refused
//! acquire leaves nothing acquired and nothing pinned.
use super::*;
use crate::phases_late::foreign_ffm::{p67_memory_session, p67_session_slots};
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// A segment carrier stamped with a fresh (shared, open) modelled session
/// in slot 2, the shape `pe_segment_session` resolves.
fn session_segment(ctx: &mut dyn NativeContext) -> (ObjectRef, ObjectRef) {
    let session = match p67_memory_session(ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    };
    let seg = alloc_segment_carrier(ctx, 8).unwrap();
    ctx.set_field(seg, 0, Value::Long(0));
    ctx.set_field(seg, 1, Value::Long(16));
    ctx.set_field(seg, PE_SEGMENT_ARENA_FIELD, Value::Object(Some(session)));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    (seg, session)
}

fn acquires(ctx: &dyn NativeContext, session: ObjectRef) -> Value {
    ctx.get_field(session, p67_session_slots(ctx, session).acquires)
}

#[test]
fn a_downcall_acquires_each_distinct_session_once_and_releases_it() {
    let mut ctx = mock_ctx();
    let (seg, session) = session_segment(&mut ctx);
    let pins_before = ctx.native_pin_count_for_test();

    let acquired = match pe_downcall_acquire_sessions(&mut ctx, &[seg, seg]) {
        Ok(acquired) => acquired,
        Err(err) => panic!("an open session refused the acquire: {err:?}"),
    };
    assert_eq!(acquired.len(), 1, "one acquire per distinct session");
    assert_eq!(acquired[0].0, session);
    assert_eq!(acquires(&ctx, session), Value::Int(1));
    assert_eq!(ctx.native_pin_count_for_test(), pins_before + 1);

    let sessions: Vec<ObjectRef> = acquired
        .iter()
        .map(|&(session, pin)| ctx.read_native_pin(pin, session))
        .collect();
    ctx.unpin_native_roots(acquired[0].1);
    pe_downcall_release_sessions(&mut ctx, &sessions);
    assert_eq!(acquires(&ctx, session), Value::Int(0));
    assert_eq!(ctx.native_pin_count_for_test(), pins_before);
}

#[test]
fn a_refused_acquire_releases_what_it_took_and_its_pins() {
    let mut ctx = mock_ctx();
    let (open_seg, open_session) = session_segment(&mut ctx);
    let (closed_seg, closed_session) = session_segment(&mut ctx);
    let slots = p67_session_slots(&ctx, closed_session);
    ctx.set_field(closed_session, slots.state, Value::Int(0));
    let pins_before = ctx.native_pin_count_for_test();

    let err = match pe_downcall_acquire_sessions(&mut ctx, &[open_seg, closed_seg]) {
        Ok(acquired) => panic!("a closed session was acquired: {acquired:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
    assert_eq!(
        acquires(&ctx, open_session),
        Value::Int(0),
        "the session acquired before the refusal must be released"
    );
    assert_eq!(acquires(&ctx, closed_session), Value::Int(0));
    assert_eq!(ctx.native_pin_count_for_test(), pins_before);
}
