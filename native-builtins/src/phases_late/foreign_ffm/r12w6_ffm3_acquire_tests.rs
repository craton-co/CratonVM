use super::*;
use crate::test_utils::mock_ctx;

fn shared_session(ctx: &mut dyn NativeContext) -> ObjectRef {
    match p67_memory_session(ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    }
}

/// Round 12 wave 6 (lane ffm3): acquires count, a close while acquired is
/// refused with the JDK's message and leaves the session OPEN, a release
/// never drives the count below zero, and an acquire after the close is
/// refused without counting.
#[test]
fn an_acquired_session_refuses_to_close_and_stays_open() {
    let mut mock = mock_ctx();
    let ctx: &mut dyn NativeContext = &mut mock;
    let mut session = shared_session(ctx);
    let slots = p67_session_slots(ctx, session);

    assert!(p67_session_acquire(ctx, &mut session).is_ok());
    assert!(p67_session_acquire(ctx, &mut session).is_ok());
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(2));

    let err = match p67_session_just_close(ctx, &mut session) {
        Ok(()) => panic!("an acquired session closed"),
        Err(err) => err,
    };
    assert!(
        format!("{err:?}").contains("Session is acquired by 2 clients"),
        "{err:?}"
    );
    assert_eq!(
        p67_session_state(ctx, session),
        1,
        "a refused close leaves it open"
    );

    p67_session_release(ctx, session);
    p67_session_release(ctx, session);
    p67_session_release(ctx, session);
    assert_eq!(
        ctx.get_field(session, slots.acquires),
        Value::Int(0),
        "a release clamps at zero"
    );

    assert!(p67_session_just_close(ctx, &mut session).is_ok());
    assert_eq!(p67_session_state(ctx, session), 0);
    let err = match p67_session_acquire(ctx, &mut session) {
        Ok(()) => panic!("a closed session was acquired"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(0));
}
