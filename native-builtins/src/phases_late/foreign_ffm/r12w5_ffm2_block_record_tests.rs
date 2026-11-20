use super::*;
use crate::test_utils::mock_ctx;

/// Round 12 wave 5 (lane ffm2): the ids an arena records live in ONE
/// `long[]` in its session's close-action list, in allocation order,
/// across the record's growth; the close walk frees them and empties the
/// record, so no id can be freed twice.
#[test]
fn recorded_blocks_are_listed_once_and_freed_by_the_close_walk() {
    let mut mock = mock_ctx();
    let ctx: &mut dyn NativeContext = &mut mock;
    let session = match p67_memory_session(ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    };
    let mut ids = Vec::new();
    // 40 crosses the record's 16 -> 32 -> 64 growth.
    for _ in 0..40 {
        let (id, _ptr) = ctx
            .allocate_native_memory(8, 8)
            .unwrap_or_else(|| panic!("mock allocation failed"));
        assert!(p67_session_record_block(ctx, session, id));
        ids.push(id);
    }
    let slots = p67_session_slots(ctx, session);
    let records: Vec<ObjectRef> = p67_session_actions(&*ctx, session)
        .into_iter()
        .filter(|&o| p67_is_block_record(&*ctx, o))
        .collect();
    assert_eq!(records.len(), 1, "exactly one record per session");
    let record = records[0];
    assert_eq!(ctx.get_array_element(record, 0), Value::Long(40));
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(ctx.get_array_element(record, i + 1), Value::Long(*id));
    }

    assert!(p67_session_run_close_actions(ctx, session).is_ok());
    assert_eq!(
        ctx.get_array_element(record, 0),
        Value::Long(0),
        "the close walk must free the recorded blocks and empty the record"
    );
    assert_eq!(
        ctx.get_field(session, slots.actions),
        Value::Object(None),
        "the list is detached before the walk"
    );
}

/// A record is a `long[]`; nothing else in the action list is one.
#[test]
fn only_a_long_array_is_a_block_record() {
    let mut mock = mock_ctx();
    let ctx: &mut dyn NativeContext = &mut mock;
    let longs = ctx.new_array(ArrayElementType::Long, 4);
    let ints = ctx.new_array(ArrayElementType::Int, 4);
    let session = match p67_memory_session(ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    };
    assert!(p67_is_block_record(ctx, longs));
    assert!(!p67_is_block_record(ctx, ints));
    assert!(!p67_is_block_record(ctx, session));
}
