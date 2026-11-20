//! Round 19 (`r16-ffm-plain-downcall-costs-microseconds`): a REAL JDK
//! session -- every `--real-jdk` arena's -- is checked, acquired and
//! released by a downcall with the JDK's own semantics.
use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeThreadAccess};

fn real_session(
    mock: &mut crate::test_utils::MockNativeContext,
    class: &str,
) -> (ObjectRef, P67SessionSlots) {
    let cid = match mock.ensure_class_initialized(class) {
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
            field(
                "resourceList",
                "Ljdk/internal/foreign/MemorySessionImpl$ResourceList;",
                0,
            ),
            field("owner", "Ljava/lang/Thread;", 1),
            field("state", "I", 2),
            field("acquireCount", "I", 3),
        ],
    );
    let ctx: &mut dyn NativeContext = mock;
    let session = ctx.alloc_object(cid, 4);
    ctx.set_field(session, 1, Value::Object(None));
    ctx.set_field(session, 2, Value::Int(0));
    ctx.set_field(session, 3, Value::Int(0));
    let slots = p67_session_slots(ctx, session);
    assert!(
        slots.real,
        "a concrete JDK session class with the JDK's fields is real"
    );
    (session, slots)
}

#[test]
fn ownerless_real_sessions_keep_their_checked_memo() {
    let mut mock = mock_ctx();
    for class in [
        "jdk/internal/foreign/SharedSession",
        "jdk/internal/foreign/GlobalSession",
    ] {
        let (session, _) = real_session(&mut mock, class);
        crate::ffm_fast::forget_validated();
        p67_note_checked_session(&mock, session, crate::ffm_fast::epoch());
        assert!(crate::ffm_fast::is_validated(session.as_ptr() as u64, false));
    }
    crate::ffm_fast::forget_validated();
}

#[test]
fn a_real_confined_close_cannot_leave_a_cached_session_verdict() {
    let mut mock = mock_ctx();
    let me = mock.stable_current_thread();
    let (session, slots) = real_session(&mut mock, "jdk/internal/foreign/ConfinedSession");
    let ctx: &mut dyn NativeContext = &mut mock;
    ctx.set_field(session, slots.owner, Value::Object(Some(me)));
    crate::ffm_fast::forget_validated();
    p67_note_checked_session(ctx, session, crate::ffm_fast::epoch());
    // This is ConfinedSession.justClose's JDK putfield, without a free or handshake.
    ctx.set_field(session, slots.state, Value::Int(-1));
    assert!(!crate::ffm_fast::is_validated(session.as_ptr() as u64, false));
    crate::ffm_fast::forget_validated();
}

#[test]
fn a_confined_session_is_checked_owner_first_and_counted() {
    let mut mock = mock_ctx();
    let me = mock.stable_current_thread();
    let (session, slots) = real_session(&mut mock, "jdk/internal/foreign/ConfinedSession");
    let ctx: &mut dyn NativeContext = &mut mock;
    ctx.set_field(session, slots.owner, Value::Object(Some(me)));
    assert_eq!(
        p67_real_session_verdict(ctx, session, slots, true),
        Some(P67RealVerdict::Open)
    );
    let mut s = session;
    assert!(p67_session_acquire(ctx, &mut s).is_ok());
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(1));
    p67_session_release(ctx, session);
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(0));
    // Closed: `Already closed`, nothing counted.
    ctx.set_field(session, slots.state, Value::Int(-1));
    assert_eq!(
        p67_real_session_verdict(ctx, session, slots, true),
        Some(P67RealVerdict::Closed)
    );
    assert!(p67_session_acquire(ctx, &mut s).is_err());
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(0));
    // Another owner: the thread error first, even when closed (JDK order).
    let other = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    ctx.set_field(session, slots.owner, Value::Object(Some(other)));
    assert_eq!(
        p67_real_session_verdict(ctx, session, slots, true),
        Some(P67RealVerdict::WrongThread)
    );
}

/// Round 19: the check's own count drives one compare-and-swap each way;
/// a count that moved since falls back to the full loop; a closing shared
/// count is refused; a `NONCLOSEABLE` session observes no count and is
/// never touched.
#[test]
fn an_observed_count_acquires_and_releases_in_one_swap() {
    let mut mock = mock_ctx();
    let (session, slots) = real_session(&mut mock, "jdk/internal/foreign/SharedSession");
    let ctx: &mut dyn NativeContext = &mut mock;
    ctx.set_field(session, slots.acquires, Value::Int(2));
    let (verdict, seen) = p67_real_session_observe(ctx, session, slots, true).unwrap();
    assert_eq!((verdict, seen.count), (P67RealVerdict::Open, Some(2)));
    assert!(p67_real_session_acquire_from(ctx, session, slots, seen));
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(3));
    p67_real_session_release_from(ctx, session, slots, seen);
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(2));
    // Moved since the check: the loop still counts once each way.
    ctx.set_field(session, slots.acquires, Value::Int(5));
    assert!(p67_real_session_acquire_from(ctx, session, slots, seen));
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(6));
    p67_real_session_release_from(ctx, session, slots, seen);
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(5));
    // A shared close in progress (count -1): refused, nothing counted.
    ctx.set_field(session, slots.acquires, Value::Int(-1));
    assert_eq!(
        p67_real_session_observe(ctx, session, slots, true).map(|(v, _)| v),
        Some(P67RealVerdict::Closed)
    );
    assert!(!p67_real_session_acquire_from(ctx, session, slots, seen));
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(-1));
    // NONCLOSEABLE: open, no count, nothing written either way.
    ctx.set_field(session, slots.state, Value::Int(P67_REAL_NONCLOSEABLE));
    ctx.set_field(session, slots.acquires, Value::Int(7));
    let (verdict, seen) = p67_real_session_observe(ctx, session, slots, true).unwrap();
    assert_eq!((verdict, seen.count), (P67RealVerdict::Open, None));
    assert!(p67_real_session_acquire_from(ctx, session, slots, seen));
    p67_real_session_release_from(ctx, session, slots, seen);
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(7));
}

#[test]
fn a_shared_session_refuses_a_closing_count_only_when_acquiring() {
    let mut mock = mock_ctx();
    let (session, slots) = real_session(&mut mock, "jdk/internal/foreign/SharedSession");
    let ctx: &mut dyn NativeContext = &mut mock;
    assert!(p67_real_session_acquire_count(ctx, session, slots));
    assert!(p67_real_session_acquire_count(ctx, session, slots));
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(2));
    p67_real_session_release_count(ctx, session, slots);
    p67_real_session_release_count(ctx, session, slots);
    p67_real_session_release_count(ctx, session, slots);
    assert_eq!(
        ctx.get_field(session, slots.acquires),
        Value::Int(0),
        "never below zero"
    );
    // `justClose` swapped the count to -1 before writing `state`.
    ctx.set_field(session, slots.acquires, Value::Int(-1));
    assert_eq!(
        p67_real_session_verdict(ctx, session, slots, true),
        Some(P67RealVerdict::Closed)
    );
    assert_eq!(
        p67_real_session_verdict(ctx, session, slots, false),
        Some(P67RealVerdict::Open)
    );
    assert!(!p67_real_session_acquire_count(ctx, session, slots));
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(-1));
}

#[test]
fn a_noncloseable_session_counts_nothing() {
    let mut mock = mock_ctx();
    let (session, slots) = real_session(&mut mock, "jdk/internal/foreign/GlobalSession");
    let ctx: &mut dyn NativeContext = &mut mock;
    ctx.set_field(session, slots.state, Value::Int(1));
    assert_eq!(
        p67_real_session_verdict(ctx, session, slots, true),
        Some(P67RealVerdict::Open)
    );
    assert!(p67_real_session_acquire_count(ctx, session, slots));
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(0));
    p67_real_session_release_count(ctx, session, slots);
    assert_eq!(ctx.get_field(session, slots.acquires), Value::Int(0));
}
