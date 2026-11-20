//! Round 13 wave 9 (lane ffm5): exactly one close wins the state flip, and
//! the global session registers nothing.
use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn shared_session(ctx: &mut dyn NativeContext) -> ObjectRef {
    match p67_memory_session(ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    }
}

fn object(ctx: &mut dyn NativeContext) -> ObjectRef {
    ctx.alloc_object(cratonvm_types::ClassId::new(0), 1)
}

/// Of two closes that both passed the liveness check, only the first
/// flips the state; the second learns it lost (and `justClose` then
/// throws `Already closed` instead of walking the list a second time).
#[test]
fn only_one_close_wins_the_state_flip() {
    let mut mock = mock_ctx();
    let ctx: &mut dyn NativeContext = &mut mock;
    let session = shared_session(ctx);
    assert_eq!(p67_session_state(ctx, session), 1);
    assert!(p67_session_flip_closed(ctx, session));
    assert_eq!(p67_session_state(ctx, session), 0);
    assert!(!p67_session_flip_closed(ctx, session));
    assert_eq!(p67_session_state(ctx, session), 0);
}

/// A close that finds the session already closed refuses before the
/// walk, so an action registered before the first close runs once.
#[test]
fn a_second_close_does_not_rerun_the_actions() {
    let mut mock = mock_ctx();
    let ctx: &mut dyn NativeContext = &mut mock;
    let mut session = shared_session(ctx);
    let block = ctx.new_array(ArrayElementType::Long, 4);
    ctx.set_array_element(block, 0, Value::Long(0));
    p67_session_push_action(ctx, session, block);
    assert!(p67_session_just_close(ctx, &mut session).is_ok());
    assert!(p67_session_run_close_actions(ctx, session).is_ok());
    let err = match p67_session_just_close(ctx, &mut session) {
        Ok(()) => panic!("a closed session closed again"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
    let slots = p67_session_slots(ctx, session);
    assert_eq!(ctx.get_field(session, slots.actions), Value::Object(None));
}

/// Two threads allocating from one shared session each fill records of
/// their own (tag in the last slot, never read as an id), every id is
/// listed exactly once, and the close walk empties every record.
#[test]
fn shared_blocks_go_into_per_thread_records() {
    let mut mock = mock_ctx();
    let ctx: &mut dyn NativeContext = &mut mock;
    let session = shared_session(ctx);
    let (tag_a, tag_b) = (7i64, 9i64);
    let mut ids: Vec<(i64, i64)> = Vec::new();
    // 40 each crosses the 16 -> 32 -> 64 growth, interleaved.
    for i in 0..80 {
        let tag = if i % 2 == 0 { tag_a } else { tag_b };
        let (id, _ptr) = ctx
            .allocate_native_memory(8, 8)
            .unwrap_or_else(|| panic!("mock allocation failed"));
        assert!(p67_session_record_block_shared(ctx, session, id, tag));
        ids.push((tag, id));
    }
    let mut listed: Vec<(i64, i64)> = Vec::new();
    let mut records = Vec::new();
    for record in p67_session_actions(ctx, session) {
        assert!(p67_is_block_record(ctx, record));
        let len = ctx.array_length(record);
        let Value::Long(tag) = ctx.get_array_element(record, len - 1) else {
            panic!("untagged record");
        };
        let Value::Long(count) = ctx.get_array_element(record, 0) else {
            panic!("no count");
        };
        assert!(
            (count as usize) + 2 <= len,
            "the count never reaches the tag"
        );
        for slot in 1..=count as usize {
            let Value::Long(id) = ctx.get_array_element(record, slot) else {
                panic!("no id");
            };
            listed.push((tag, id));
        }
        records.push(record);
    }
    assert!(records.len() >= 4, "both threads grew past one record");
    listed.sort_unstable();
    ids.sort_unstable();
    assert_eq!(listed, ids, "every id once, under its own thread's tag");

    assert!(p67_session_run_close_actions(ctx, session).is_ok());
    for record in records {
        assert_eq!(ctx.get_array_element(record, 0), Value::Long(0));
    }
}

/// `GlobalSession.addInternal` does nothing: a `reinterpret(.., global,
/// cleanup)` cleanup and an `addCloseAction` leave the global session's
/// list empty, while any other session still records them.
#[test]
fn the_global_session_registers_nothing() {
    const VM: usize = 0x7A80_0911;
    let mut mock = mock_ctx();
    mock.set_vm_identity(VM);
    let ctx: &mut dyn NativeContext = &mut mock;
    let arena = p67_global_arena(ctx).expect("global arena");
    let mut global = p67_arena_session(ctx, arena).expect("a modelled global session");
    let (cleanup, segment, action) = (object(ctx), object(ctx), object(ctx));
    assert!(p67_session_add_cleanup(ctx, &mut global, cleanup, segment).is_ok());
    let args = [Value::Object(Some(global)), Value::Object(Some(action))];
    assert!(p67_session_add_action_synthetic(ctx, &args, false).is_ok());
    let slots = p67_session_slots(ctx, global);
    assert_eq!(ctx.get_field(global, slots.actions), Value::Object(None));

    let mut other = shared_session(ctx);
    assert!(p67_session_add_cleanup(ctx, &mut other, cleanup, segment).is_ok());
    let args = [Value::Object(Some(other)), Value::Object(Some(action))];
    assert!(p67_session_add_action_synthetic(ctx, &args, false).is_ok());
    assert_eq!(
        p67_session_actions(ctx, other).len(),
        2,
        "an ordinary session records both"
    );
    crate::panama::forget_vm_ffm_singletons(VM);
}
