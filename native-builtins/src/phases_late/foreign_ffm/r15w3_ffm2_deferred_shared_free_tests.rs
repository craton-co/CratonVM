//! Round 15 wave 3 (lane ffm2): the close walk hands block ids over to the
//! deferred shared free instead of freeing them.
use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn session(ctx: &mut dyn NativeContext) -> ObjectRef {
    match p67_memory_session(ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    }
}

/// With `deferred` given, the walk moves every recorded id into it (in
/// either walk order) and empties the record, so nothing is freed twice.
#[test]
fn the_walk_hands_the_ids_over_instead_of_freeing_them() {
    let mut mock = mock_ctx();
    let ctx: &mut dyn NativeContext = &mut mock;
    let session = session(ctx);
    let mut ids = Vec::new();
    for _ in 0..3 {
        let (id, _ptr) = ctx
            .allocate_native_memory(8, 8)
            .unwrap_or_else(|| panic!("mock allocation failed"));
        assert!(p67_session_record_block(ctx, session, id));
        ids.push(id);
    }
    let record = p67_session_actions(&*ctx, session)
        .into_iter()
        .find(|&o| p67_is_block_record(&*ctx, o))
        .unwrap_or_else(|| panic!("no block record"));
    let slots = p67_session_slots(&*ctx, session);
    let Value::Object(Some(actions)) = ctx.get_field(session, slots.actions) else {
        panic!("no action list");
    };
    ctx.set_field(session, slots.actions, Value::Object(None));
    let mut deferred = Some(Vec::new());
    assert!(p67_session_walk_actions(ctx, actions, false, &mut deferred).is_ok());
    let mut handed = deferred.unwrap_or_else(|| panic!("taken"));
    handed.sort_unstable();
    ids.sort_unstable();
    assert_eq!(handed, ids);
    assert_eq!(ctx.get_array_element(record, 0), Value::Long(0), "emptied");
}

/// `p67_take_block_record_ids` reads `[1..=count]` only and empties.
#[test]
fn taking_the_ids_empties_the_record() {
    let mut mock = mock_ctx();
    let ctx: &mut dyn NativeContext = &mut mock;
    let record = ctx.new_array(ArrayElementType::Long, 5);
    ctx.set_array_element(record, 0, Value::Long(2));
    ctx.set_array_element(record, 1, Value::Long(11));
    ctx.set_array_element(record, 2, Value::Long(12));
    ctx.set_array_element(record, 4, Value::Long(99));
    assert_eq!(p67_take_block_record_ids(ctx, record), vec![11, 12]);
    assert_eq!(ctx.get_array_element(record, 0), Value::Long(0));
    assert!(
        p67_take_block_record_ids(ctx, record).is_empty(),
        "never twice"
    );
}
