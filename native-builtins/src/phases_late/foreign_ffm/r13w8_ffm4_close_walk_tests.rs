//! Round 13 wave 8 (lane ffm4): the close walk's JDK order, suppressed
//! failures, blocks freed after every cleanup, and `addOrCleanupIfFail`
//! cleaning up a refused resource.
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::{Cell, RefCell};

thread_local! {
    /// Receivers of `run` / `accept` / `cleanup`, in call order.
    static RAN: RefCell<Vec<ObjectRef>> = const { RefCell::new(Vec::new()) };
    /// `(receiver, throwable)`: what each receiver throws.
    static THROWS: RefCell<Vec<(ObjectRef, ObjectRef)>> = const { RefCell::new(Vec::new()) };
    /// `(first, suppressed)` pairs `addSuppressed` saw.
    static SUPPRESSED: RefCell<Vec<(ObjectRef, ObjectRef)>> = const { RefCell::new(Vec::new()) };
    /// A block record whose count `accept` samples.
    static RECORD: Cell<Option<ObjectRef>> = const { Cell::new(None) };
    static COUNT_AT_ACCEPT: Cell<i64> = const { Cell::new(-1) };
}

fn reset() {
    RAN.with(|r| r.borrow_mut().clear());
    THROWS.with(|t| t.borrow_mut().clear());
    SUPPRESSED.with(|s| s.borrow_mut().clear());
    RECORD.with(|c| c.set(None));
    COUNT_AT_ACCEPT.with(|c| c.set(-1));
}

fn hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match method {
        "run" | "accept" | "cleanup" => {
            RAN.with(|r| r.borrow_mut().push(receiver));
            if let Some(record) = RECORD.with(|c| c.get()) {
                if let Value::Long(n) = ctx.get_array_element(record, 0) {
                    COUNT_AT_ACCEPT.with(|c| c.set(n));
                }
            }
            let thrown = THROWS.with(|t| {
                t.borrow()
                    .iter()
                    .find(|(r, _)| *r == receiver)
                    .map(|(_, e)| *e)
            });
            Some(match thrown {
                Some(e) => Err(MethodCallFailed::ExceptionThrown(e)),
                None => Ok(None),
            })
        }
        "addSuppressed" => {
            if let Some(Value::Object(Some(s))) = args.first() {
                SUPPRESSED.with(|v| v.borrow_mut().push((receiver, *s)));
            }
            Some(Ok(None))
        }
        _ => None,
    }
}

fn session(ctx: &mut dyn NativeContext, confined: bool) -> ObjectRef {
    let session = match p67_memory_session(ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    };
    if confined {
        let owner = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
        let slots = p67_session_slots(ctx, session);
        ctx.set_field(session, slots.owner, Value::Object(Some(owner)));
    }
    session
}

fn object(ctx: &mut dyn NativeContext) -> ObjectRef {
    ctx.alloc_object(cratonvm_types::ClassId::new(0), 1)
}

#[test]
fn the_jdk_order_is_cache_first_for_confined_and_newest_first_for_shared() {
    assert_eq!(p67_close_order(0, true), Vec::<usize>::new());
    assert_eq!(p67_close_order(1, true), vec![0]);
    assert_eq!(p67_close_order(4, true), vec![0, 3, 2, 1]);
    assert_eq!(p67_close_order(4, false), vec![3, 2, 1, 0]);
}

/// Three actions on a confined session run 1-3-2 (the JDK's confined
/// list keeps its first action in a cache slot, then runs its stack); on
/// a shared one 3-2-1. Each runs once, and the list is detached.
#[test]
fn the_walk_runs_each_action_once_in_the_jdk_order() {
    for confined in [true, false] {
        reset();
        let mut mock = mock_ctx();
        mock.set_invoke_virtual_hook(hook);
        let ctx: &mut dyn NativeContext = &mut mock;
        let session = session(ctx, confined);
        let actions: Vec<ObjectRef> = (0..3).map(|_| object(ctx)).collect();
        for action in &actions {
            p67_session_push_action(ctx, session, *action);
        }
        assert!(p67_session_run_close_actions(ctx, session).is_ok());
        let want = if confined {
            vec![actions[0], actions[2], actions[1]]
        } else {
            vec![actions[2], actions[1], actions[0]]
        };
        assert_eq!(
            RAN.with(|r| r.borrow().clone()),
            want,
            "confined: {confined}"
        );
        let slots = p67_session_slots(ctx, session);
        assert_eq!(ctx.get_field(session, slots.actions), Value::Object(None));
    }
}

/// The first failure is rethrown; a later DISTINCT one is attached to it
/// with `addSuppressed`, the same one again is not (no self-suppression),
/// and every action still runs.
#[test]
fn later_failures_are_suppressed_into_the_first() {
    reset();
    let mut mock = mock_ctx();
    mock.set_invoke_virtual_hook(hook);
    let ctx: &mut dyn NativeContext = &mut mock;
    let session = session(ctx, false);
    let (a, b, c) = (object(ctx), object(ctx), object(ctx));
    let (e1, e3) = (object(ctx), object(ctx));
    for action in [a, b, c] {
        p67_session_push_action(ctx, session, action);
    }
    THROWS.with(|t| t.borrow_mut().extend([(c, e3), (b, e3), (a, e1)]));
    match p67_session_run_close_actions(ctx, session) {
        Err(MethodCallFailed::ExceptionThrown(got)) => assert_eq!(got, e3),
        other => panic!("expected the first failure, got {other:?}"),
    }
    assert_eq!(RAN.with(|r| r.borrow().clone()), vec![c, b, a]);
    assert_eq!(SUPPRESSED.with(|s| s.borrow().clone()), vec![(e3, e1)]);
}

/// On a confined session whose FIRST action is the block record (the
/// arena allocated before registering the cleanup), the cleanup still
/// runs while the block is recorded, and the block is freed after it.
#[test]
fn blocks_are_freed_after_every_cleanup_even_from_the_cache_slot() {
    reset();
    let mut mock = mock_ctx();
    mock.set_invoke_virtual_hook(hook);
    let ctx: &mut dyn NativeContext = &mut mock;
    // Built shared (the mock's thread object is not stable, so an owned
    // session would refuse the add), then given an owner for the walk.
    let mut session = session(ctx, false);
    let (id, _ptr) = ctx
        .allocate_native_memory(16, 8)
        .unwrap_or_else(|| panic!("mock allocation failed"));
    assert!(p67_session_record_block(ctx, session, id));
    let slots = p67_session_slots(ctx, session);
    let Some(&record) = p67_session_actions(ctx, session).first() else {
        panic!("no block record");
    };
    assert!(p67_is_block_record(ctx, record));
    RECORD.with(|c| c.set(Some(record)));
    let cleanup = object(ctx);
    let segment = object(ctx);
    assert!(p67_session_add_cleanup(ctx, &mut session, cleanup, segment).is_ok());
    let owner = object(ctx);
    ctx.set_field(session, slots.owner, Value::Object(Some(owner)));
    assert!(p67_session_run_close_actions(ctx, session).is_ok());
    assert_eq!(RAN.with(|r| r.borrow().clone()), vec![cleanup]);
    assert_eq!(
        COUNT_AT_ACCEPT.with(|c| c.get()),
        1,
        "the block was live at accept"
    );
    assert_eq!(
        ctx.get_array_element(record, 0),
        Value::Long(0),
        "then freed"
    );
}

/// `addOrCleanupIfFail` on a closed session runs the resource's
/// `cleanup()` and rethrows the refusal; `addInternal` only rethrows.
#[test]
fn a_refused_add_or_cleanup_if_fail_cleans_up_then_rethrows() {
    for cleanup_if_fail in [true, false] {
        reset();
        let mut mock = mock_ctx();
        mock.set_invoke_virtual_hook(hook);
        let ctx: &mut dyn NativeContext = &mut mock;
        let session = session(ctx, false);
        let slots = p67_session_slots(ctx, session);
        ctx.set_field(session, slots.state, Value::Int(0));
        let resource = object(ctx);
        let args = [Value::Object(Some(session)), Value::Object(Some(resource))];
        let err = match p67_session_add_action_synthetic(ctx, &args, cleanup_if_fail) {
            Ok(_) => panic!("a closed session accepted an action"),
            Err(err) => err,
        };
        assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
        let want = if cleanup_if_fail {
            vec![resource]
        } else {
            Vec::new()
        };
        assert_eq!(RAN.with(|r| r.borrow().clone()), want);
        assert_eq!(ctx.get_field(session, slots.actions), Value::Object(None));
    }
}

/// Two appends keep both actions, in order (the CAS path).
#[test]
fn appends_keep_every_action_in_order() {
    let mut mock = mock_ctx();
    let ctx: &mut dyn NativeContext = &mut mock;
    let session = session(ctx, false);
    let (a, b) = (object(ctx), object(ctx));
    p67_session_push_action(ctx, session, a);
    p67_session_push_action(ctx, session, b);
    assert_eq!(p67_session_actions(ctx, session), vec![a, b]);
}
