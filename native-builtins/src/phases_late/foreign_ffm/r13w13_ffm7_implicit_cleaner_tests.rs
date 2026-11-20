//! Round 13 wave 13 (lane ffm7): an `Arena.ofAuto()` session's close
//! actions are registered with the JDK's `Cleaner`
//! (`r12w7-ffm4-reinterpret-cleanup-residuals` item 2).
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::RefCell;

thread_local! {
    /// Every virtual call the hook saw: receiver, method, arguments.
    static CALLS: RefCell<Vec<(ObjectRef, String, Vec<Value>)>> = const { RefCell::new(Vec::new()) };
}

/// Records every virtual call; answers `Cleaner.register` with `null`
/// and leaves the rest to the mock's default.
fn recording_hook(
    _ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    CALLS.with(|c| {
        c.borrow_mut()
            .push((receiver, method.to_string(), args.to_vec()))
    });
    (method == "register").then_some(Ok(Some(Value::Object(None))))
}

fn calls() -> Vec<(ObjectRef, String, Vec<Value>)> {
    CALLS.with(|c| c.borrow().clone())
}

fn session(ctx: &mut dyn NativeContext) -> ObjectRef {
    match p67_memory_session(ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    }
}

fn cleaner(ctx: &mut dyn NativeContext) -> ObjectRef {
    let cid = match ctx.ensure_class_initialized("java/lang/ref/Cleaner") {
        Ok(cid) => cid,
        Err(err) => panic!("no class: {err:?}"),
    };
    ctx.alloc_object(cid, 1)    }

/// A marked session answers its `Cleaner`; an `addCloseAction` on it is
/// `cleaner.register(session, action)` and leaves the list holding only
/// the marker. Unmarked sessions, and a list whose one entry is not a
/// `Cleaner`, answer no cleaner.
#[test]
fn an_implicit_sessions_action_is_registered_with_its_cleaner() {
    CALLS.with(|c| c.borrow_mut().clear());
    let mut mock = mock_ctx();
    mock.set_invoke_virtual_hook(recording_hook);
    let pins_before = mock.native_pin_count_for_test();
    let ctx: &mut dyn NativeContext = &mut mock;
    let plain = session(ctx);
    assert_eq!(p67_implicit_session_cleaner(ctx, plain), None);
    let other = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    p67_session_push_action(ctx, plain, other);
    assert_eq!(
        p67_implicit_session_cleaner(ctx, plain),
        None,
        "not a Cleaner"
    );

    let session = session(ctx);
    let cleaner = cleaner(ctx);
    assert!(p67_is_cleaner_marker(ctx, cleaner));
    let session = p67_mark_implicit_session(ctx, session, cleaner);
    assert_eq!(p67_implicit_session_cleaner(ctx, session), Some(cleaner));

    let action = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let added = p67_session_add_action_synthetic(
        ctx,
        &[Value::Object(Some(session)), Value::Object(Some(action))],
        false,
    );
    assert!(added.is_ok(), "{added:?}");
    let seen = calls();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].0, cleaner);
    assert_eq!(seen[0].1, "register");
    assert_eq!(
        seen[0].2,
        vec![Value::Object(Some(session)), Value::Object(Some(action))]
    );
    assert_eq!(p67_session_actions(ctx, session), vec![cleaner]);
    assert_eq!(p67_implicit_session_cleaner(ctx, session), Some(cleaner));
    assert_eq!(mock.native_pin_count_for_test(), pins_before);
}

/// A `reinterpret` cleanup whose `UniAccept` carrier cannot be built (the
/// mock runs no `CompletableFuture.completedFuture`) falls back to the
/// list, as before this wave; the session then no longer answers a
/// cleaner, so later adds take the list too.
#[test]
fn a_cleanup_without_a_carrier_falls_back_to_the_list() {
    CALLS.with(|c| c.borrow_mut().clear());
    let mut mock = mock_ctx();
    mock.set_invoke_virtual_hook(recording_hook);
    let pins_before = mock.native_pin_count_for_test();
    let ctx: &mut dyn NativeContext = &mut mock;
    let session = session(ctx);
    let cleaner = cleaner(ctx);
    let mut session = p67_mark_implicit_session(ctx, session, cleaner);
    let cleanup = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let cleanup_seg = ctx.alloc_object(cratonvm_types::ClassId::new(0), 6);
    assert!(p67_session_add_cleanup(ctx, &mut session, cleanup, cleanup_seg).is_ok());
    assert!(
        !calls().iter().any(|(_, method, _)| method == "register"),
        "nothing registered without a carrier"
    );
    let actions = p67_session_actions(ctx, session);
    assert_eq!(actions.len(), 2, "the marker, then the record");
    assert_eq!(actions[0], cleaner);
    assert_eq!(
        p67_cleanup_record(ctx, actions[1]),
        Some((cleanup, cleanup_seg))
    );
    assert_eq!(p67_implicit_session_cleaner(ctx, session), None);
    assert_eq!(mock.native_pin_count_for_test(), pins_before);
}

/// A close walk over a marked session's list never calls the marker (it
/// is no action), and still runs what else the list holds.
#[test]
fn the_close_walk_skips_the_marker() {
    CALLS.with(|c| c.borrow_mut().clear());
    let mut mock = mock_ctx();
    mock.set_invoke_virtual_hook(recording_hook);
    let ctx: &mut dyn NativeContext = &mut mock;
    let session = session(ctx);
    let cleaner = cleaner(ctx);
    let session = p67_mark_implicit_session(ctx, session, cleaner);
    let runnable = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    p67_session_push_action(ctx, session, runnable);
    assert!(p67_session_run_close_actions(ctx, session).is_ok());
    let seen = calls();
    assert!(
        !seen.iter().any(|(receiver, _, _)| *receiver == cleaner),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|(receiver, _, _)| *receiver == runnable),
        "{seen:?}"
    );
}
