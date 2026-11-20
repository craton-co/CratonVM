use super::*;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::NativeHeapAccess;
use cratonvm_types::ArrayElementType;

/// Slot the counting hook keeps its tally in, on the action object itself —
/// the hook is a plain `fn` and cannot capture a counter.
const ACTION_RUN_COUNT_SLOT: usize = 0;

fn count_run_calls(
    ctx: &mut crate::test_utils::MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "run" && descriptor == "()V" {
        let prev = match ctx.get_field(receiver, ACTION_RUN_COUNT_SLOT) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(receiver, ACTION_RUN_COUNT_SLOT, Value::Int(prev + 1));
        return Some(Ok(None));
    }
    None
}

fn failing_run(
    _ctx: &mut crate::test_utils::MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "run" && descriptor == "()V" {
        return Some(Err(RuntimeError::IllegalStateException {
            message: "barrier action blew up".to_string(),
        }
        .into()));
    }
    None
}

fn new_barrier_obj(ctx: &mut crate::test_utils::MockNativeContext) -> ObjectRef {
    match ctx.new_object("java/util/concurrent/CyclicBarrier") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("expected a receiver, got {other:?}"),
    }
}

fn runs(ctx: &crate::test_utils::MockNativeContext, action: ObjectRef) -> i32 {
    match ctx.get_field(action, ACTION_RUN_COUNT_SLOT) {
        Value::Int(v) => v,
        _ => 0,
    }
}

/// The `Runnable` handed to `CyclicBarrier(int, Runnable)` must run once per
/// trip, on the last party in.
///
/// It used to be discarded by the constructor — `native_cb_init_action` was
/// `native_cb_init` with a "Simplified: ignore the barrier action" comment —
/// so `barrierActionRuns` was 0 where HotSpot says 1. One party is enough to
/// measure it: `await()` with `parties == 1` IS the last arrival.
#[test]
fn barrier_action_runs_once_per_trip() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(count_run_calls);

    let action = new_barrier_obj(&mut ctx);
    let barrier = new_barrier_obj(&mut ctx);
    native_cb_init_action(
        &mut ctx,
        &[
            Value::Object(Some(barrier)),
            Value::Int(1),
            Value::Object(Some(action)),
        ],
    )
    .unwrap();

    for expected in 1..=3 {
        let idx = native_cb_await(&mut ctx, &[Value::Object(Some(barrier))])
            .unwrap()
            .unwrap();
        assert_eq!(idx, Value::Int(0), "the last arrival's index is 0");
        assert_eq!(
            runs(&ctx, action),
            expected,
            "the barrier action must run once per trip"
        );
    }
}

/// The ONE-arg constructor has no action, and must not invent one by reading
/// whatever the holder's action slot happens to hold.
#[test]
fn barrier_without_an_action_runs_nothing() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(count_run_calls);

    let action = new_barrier_obj(&mut ctx);
    let barrier = new_barrier_obj(&mut ctx);
    native_cb_init(&mut ctx, &[Value::Object(Some(barrier)), Value::Int(1)]).unwrap();
    native_cb_await(&mut ctx, &[Value::Object(Some(barrier))]).unwrap();
    assert_eq!(runs(&ctx, action), 0);
}

/// The action is stored in a REFERENCE slot of a reference array, not punned
/// into a slot the class declares as an `int`.
///
/// That is the whole reason the holder gained a level: the receiver's slot 0
/// is `lock` in the real layout and an untyped stub slot in the synthetic
/// one, and parking a `Runnable` in a second slot would be the shape the
/// GC's W7-84 guard reports and auto-boxes.
#[test]
fn holder_keeps_state_and_action_in_typed_arrays() {
    let mut ctx = mock_ctx();
    let action = new_barrier_obj(&mut ctx);
    let barrier = new_barrier_obj(&mut ctx);
    native_cb_init_action(
        &mut ctx,
        &[
            Value::Object(Some(barrier)),
            Value::Int(4),
            Value::Object(Some(action)),
        ],
    )
    .unwrap();

    let holder = match ctx.get_field(barrier, CB_FIELD_PARTIES) {
        Value::Object(Some(h)) => h,
        other => panic!("expected the holder in slot 0, got {other:?}"),
    };
    assert_eq!(
        ctx.heap_element_type_of(holder),
        ArrayElementType::Reference
    );
    assert_eq!(ctx.array_length(holder), CB_HOLDER_LEN);
    assert_eq!(
        ctx.get_array_element(holder, CB_HOLDER_ACTION),
        Value::Object(Some(action))
    );

    let state = cb_state(&ctx, holder).expect("state array");
    assert_eq!(ctx.heap_element_type_of(state), ArrayElementType::Long);
    assert_eq!(ctx.array_length(state), CB_STATE_LEN);
    assert_eq!(cb_get(&ctx, state, CB_H_PARTIES), 4);
    assert_eq!(cb_get(&ctx, state, CB_H_BROKEN_GEN), CB_NO_BREAK);
}

/// A trip opens a NEW generation instead of only clearing `count`.
///
/// The old release test was "`count` is back to 0", which is only valid
/// while nobody re-enters the barrier: a released waiter preempted before it
/// re-read `count` would find a faster party had already bumped it, decide
/// it had not been released, and park again with its wake-up spent — one
/// party short, and every later trip deadlocked. The generation only ever
/// moves forward, so a waiter's "did my generation end" test cannot be
/// undone by the next round starting.
#[test]
fn a_trip_advances_the_generation() {
    let mut ctx = mock_ctx();
    let barrier = new_barrier_obj(&mut ctx);
    native_cb_init(&mut ctx, &[Value::Object(Some(barrier)), Value::Int(1)]).unwrap();
    let holder = match ctx.get_field(barrier, CB_FIELD_PARTIES) {
        Value::Object(Some(h)) => h,
        other => panic!("expected the holder, got {other:?}"),
    };
    let state = cb_state(&ctx, holder).expect("state array");

    assert_eq!(cb_get(&ctx, state, CB_H_GENERATION), 0);
    native_cb_await(&mut ctx, &[Value::Object(Some(barrier))]).unwrap();
    assert_eq!(cb_get(&ctx, state, CB_H_GENERATION), 1);
    assert_eq!(cb_get(&ctx, state, CB_H_COUNT), 0);
    native_cb_await(&mut ctx, &[Value::Object(Some(barrier))]).unwrap();
    assert_eq!(cb_get(&ctx, state, CB_H_GENERATION), 2);
}

/// `reset()` breaks the generation its parked parties are waiting in, and
/// still reports `isBroken() == false` — the two are not in conflict, they
/// are about different generations.
#[test]
fn reset_breaks_the_old_generation_and_reports_unbroken() {
    let mut ctx = mock_ctx();
    let barrier = new_barrier_obj(&mut ctx);
    native_cb_init(&mut ctx, &[Value::Object(Some(barrier)), Value::Int(3)]).unwrap();
    let holder = match ctx.get_field(barrier, CB_FIELD_PARTIES) {
        Value::Object(Some(h)) => h,
        other => panic!("expected the holder, got {other:?}"),
    };
    let state = cb_state(&ctx, holder).expect("state array");
    // Two of three parties have arrived and are parked.
    cb_set(&ctx, state, CB_H_COUNT, 2);

    native_cb_reset(&mut ctx, &[Value::Object(Some(barrier))]).unwrap();

    assert_eq!(
        cb_get(&ctx, state, CB_H_BROKEN_GEN),
        0,
        "generation 0 — the one the parked parties are in — must be broken"
    );
    assert_eq!(cb_get(&ctx, state, CB_H_GENERATION), 1);
    assert_eq!(
        native_cb_is_broken(&mut ctx, &[Value::Object(Some(barrier))]).unwrap(),
        Some(Value::Int(0)),
        "the FRESH generation is not broken"
    );
    assert_eq!(
        native_cb_get_number_waiting(&mut ctx, &[Value::Object(Some(barrier))]).unwrap(),
        Some(Value::Int(0))
    );
}

/// An action that throws breaks the barrier and propagates, rather than
/// letting the parties past a trip whose action did not complete.
#[test]
fn a_failing_barrier_action_breaks_the_barrier() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(failing_run);

    let action = new_barrier_obj(&mut ctx);
    let barrier = new_barrier_obj(&mut ctx);
    native_cb_init_action(
        &mut ctx,
        &[
            Value::Object(Some(barrier)),
            Value::Int(1),
            Value::Object(Some(action)),
        ],
    )
    .unwrap();

    assert!(
        native_cb_await(&mut ctx, &[Value::Object(Some(barrier))]).is_err(),
        "the action's failure must reach the caller"
    );
    assert_eq!(
        native_cb_is_broken(&mut ctx, &[Value::Object(Some(barrier))]).unwrap(),
        Some(Value::Int(1))
    );
    // ...and the barrier stays broken for the next party in.
    assert!(native_cb_await(&mut ctx, &[Value::Object(Some(barrier))]).is_err());
}

/// `parties <= 0` is rejected by BOTH constructors.
#[test]
fn zero_parties_is_rejected_by_both_constructors() {
    let mut ctx = mock_ctx();
    let barrier = new_barrier_obj(&mut ctx);
    assert!(native_cb_init(&mut ctx, &[Value::Object(Some(barrier)), Value::Int(0)]).is_err());
    assert!(native_cb_init_action(
        &mut ctx,
        &[
            Value::Object(Some(barrier)),
            Value::Int(-1),
            Value::Object(None)
        ]
    )
    .is_err());
}
