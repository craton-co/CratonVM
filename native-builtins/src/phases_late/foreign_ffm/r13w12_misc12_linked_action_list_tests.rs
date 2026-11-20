//! Round 13 wave 12 (lane misc12): the close-action list is a linked stack
//! (`CRATONVM_FFM_ACTION_LIST_LINKED`), O(1) per add, read back in
//! registration order, with the block record found through the head's
//! hint rather than a walk.
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

fn head(ctx: &dyn NativeContext, session: ObjectRef) -> ObjectRef {
    let slots = p67_session_slots(ctx, session);
    match ctx.get_field(session, slots.actions) {
        Value::Object(Some(head)) => head,
        other => panic!("no action list: {other:?}"),
    }
}

/// Many adds keep every action, in order, and the list is a stack of
/// nodes (the head is one node, not an array as long as the list).
#[test]
fn many_adds_are_linked_and_read_back_in_registration_order() {
    let mut mock = mock_ctx();
    let ctx: &mut dyn NativeContext = &mut mock;
    let session = shared_session(ctx);
    let actions: Vec<ObjectRef> = (0..5_000).map(|_| object(ctx)).collect();
    for &action in &actions {
        p67_session_push_action(ctx, session, action);
    }
    let head = head(ctx, session);
    assert!(p67_is_action_node(ctx, head), "the head is a node");
    assert_eq!(ctx.array_length(head), P67_NODE_LEN);
    assert_eq!(p67_session_actions(ctx, session), actions);
    // The walk's view: one registration-ordered array.
    let array = p67_action_list_as_array(ctx, head);
    assert_eq!(ctx.array_length(array), actions.len());
    assert_eq!(
        ctx.get_array_element(array, 0),
        Value::Object(Some(actions[0]))
    );
    assert_eq!(
        ctx.get_array_element(array, actions.len() - 1),
        Value::Object(Some(actions[actions.len() - 1]))
    );
}


/// The block record registered by the first allocation is found through
/// the head's hint after later cleanups, grows in place in its node, and
/// stays the one record, oldest in the list; the close walk frees it.
#[test]
fn the_block_record_is_found_through_the_hint_after_later_cleanups() {
    let mut mock = mock_ctx();
    let ctx: &mut dyn NativeContext = &mut mock;
    let session = shared_session(ctx);
    let mut ids = Vec::new();
    let mut cleanups = Vec::new();
    // 40 ids cross the record's 16 -> 32 -> 64 growth; a cleanup after
    // every id puts 40 nodes in front of the record.
    for _ in 0..40 {
        let (id, _ptr) = ctx
            .allocate_native_memory(8, 8)
            .unwrap_or_else(|| panic!("mock allocation failed"));
        assert!(p67_session_record_block(ctx, session, id));
        ids.push(id);
        let cleanup = object(ctx);
        p67_session_push_action(ctx, session, cleanup);
        cleanups.push(cleanup);
    }
    let listed = p67_session_actions(ctx, session);
    assert_eq!(listed.len(), 41, "one record and 40 cleanups");
    let record = listed[0];
    assert!(
        p67_is_block_record(ctx, record),
        "the record is the oldest action"
    );
    assert_eq!(&listed[1..], &cleanups[..]);
    assert_eq!(ctx.get_array_element(record, 0), Value::Long(40));
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(ctx.get_array_element(record, i + 1), Value::Long(*id));
    }
    let head = head(ctx, session);
    let hinted = p67_node_ref(ctx, head, P67_NODE_RECORD).expect("a record hint");
    assert_eq!(p67_node_ref(ctx, hinted, P67_NODE_ACTION), Some(record));

    // The walk frees the record (the cleanups are plain objects the mock
    // answers `run` for with nothing).
    let _ = p67_session_run_close_actions(ctx, session);
    assert_eq!(ctx.get_array_element(record, 0), Value::Long(0));
}
