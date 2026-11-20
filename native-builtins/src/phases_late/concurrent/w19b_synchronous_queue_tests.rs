use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const SQ: &str = "java/util/concurrent/SynchronousQueue";
const OFFER_TIMED: &str = "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;)Z";
const POLL_TIMED: &str = "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;";

struct Teardown(usize);
impl Drop for Teardown {
    fn drop(&mut self) {
        forget_vm_concurrent_tables(self.0);
    }
}

fn synthetic_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_p58_synchronous_queue(&mut r);
    // `concurrent_extras` (which ran after phase 58) is deleted
    // (interpreter round i4 wave 84, lane L6); it registered no
    // `SynchronousQueue` triple since w19-b.
    r
}

fn native(name: &str, desc: &str) -> cratonvm_native_api::NativeCallback {
    synthetic_registry()
        .find(SQ, name, desc)
        .unwrap_or_else(|| panic!("SynchronousQueue.{name}{desc} is registered"))
}

fn mock_on(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

fn object(ctx: &mut MockNativeContext, fields: usize) -> ObjectRef {
    ctx.alloc_object(cratonvm_types::ClassId::new(0), fields)
}

/// A synthetic `TimeUnit` whose ordinal sits in slot 0.
fn time_unit(ctx: &mut MockNativeContext, ordinal: i32) -> ObjectRef {
    let unit = object(ctx, 1);
    ctx.set_field(unit, 0, Value::Int(ordinal));
    unit
}

fn is_interrupted_exception(e: &cratonvm_types::error::MethodCallFailed) -> bool {
    matches!(
        e,
        cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::InterruptedException
            )
        )
    )
}

/// Run `body` on its own thread and fail, rather than hang, if it does not
/// return: a parked native whose interrupt handling regressed spins for
/// ever against the mock's `park`.
fn bounded(what: &'static str, body: impl FnOnce() + Send + 'static) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        body();
        let _ = tx.send(());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(20)) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("{what} did not return: an interrupted waiter spins")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("{what} panicked (see its message above)")
        }
    }
}

/// Every `SynchronousQueue` triple the two passes ever registered is now
/// the phase-58 one.
#[test]
fn w19b_every_triple_dispatches_to_the_one_table() {
    let r = synthetic_registry();
    let expected: [(&str, &str, cratonvm_native_api::NativeCallback); 9] = [
        ("<init>", "()V", p58_sq_init),
        ("<init>", "(Z)V", p58_sq_init_fair),
        ("put", "(Ljava/lang/Object;)V", p58_sq_put),
        ("offer", "(Ljava/lang/Object;)Z", p58_sq_offer),
        ("offer", OFFER_TIMED, p58_sq_offer_timed),
        ("take", "()Ljava/lang/Object;", p58_sq_take),
        ("poll", "()Ljava/lang/Object;", p58_sq_poll),
        ("poll", POLL_TIMED, p58_sq_poll_timed),
        ("clear", "()V", p58_sq_clear),
    ];
    for (name, desc, want) in expected {
        let got = r
            .find(SQ, name, desc)
            .unwrap_or_else(|| panic!("SynchronousQueue.{name}{desc} is registered"));
        assert_eq!(
            got as usize, want as usize,
            "SynchronousQueue.{name}{desc} must dispatch to the phase-58 table"
        );
    }
}

/// The defect: a timed `offer` found no consumer because `take` waited in
/// the other table. Now it hands straight to the blocked `take`.
#[test]
fn w19b_a_timed_offer_meets_a_blocked_take() {
    const VM: usize = 0x19B0_0A01;
    let _teardown = Teardown(VM);
    let mut ctx = mock_on(VM);
    let queue = object(&mut ctx, 2);
    let item = object(&mut ctx, 1);
    let seconds = time_unit(&mut ctx, 3);
    let key = ctx.identity_hash_code(queue);
    let baseline = ctx.global_root_count();

    let SqConsumed::Filed(root, ticket) = sq_consume(&mut ctx, queue, key, true) else {
        panic!("no producer waits, so the taker is filed");
    };
    let offered = native("offer", OFFER_TIMED)(
        &mut ctx,
        &[
            Value::Object(Some(queue)),
            Value::Object(Some(item)),
            Value::Long(1),
            Value::Object(Some(seconds)),
        ],
    )
    .unwrap();
    assert_eq!(offered, Some(Value::Int(1)), "handed to the blocked take");
    assert_eq!(
        sq_consumer_collect(&mut ctx, key, root, ticket, false),
        Some(Value::Object(Some(item))),
        "the woken take receives the element"
    );
    assert_eq!(ctx.global_root_count(), baseline, "nothing stays rooted");
    assert!(SQ_TABLE.peek(VM, |t| t.is_empty()).unwrap_or(true));
}

/// `put` hands to a blocked `take`; `take` and a timed `poll` each take a
/// blocked producer's element -- all through the one table.
#[test]
fn w19b_put_take_and_timed_poll_meet_their_partners() {
    const VM: usize = 0x19B0_0A02;
    let _teardown = Teardown(VM);
    let mut ctx = mock_on(VM);
    let queue = object(&mut ctx, 2);
    let first = object(&mut ctx, 1);
    let second = object(&mut ctx, 1);
    let third = object(&mut ctx, 1);
    let millis = time_unit(&mut ctx, 2);
    let key = ctx.identity_hash_code(queue);
    let baseline = ctx.global_root_count();

    // put -> a blocked take.
    let SqConsumed::Filed(root, ticket) = sq_consume(&mut ctx, queue, key, true) else {
        panic!("filed");
    };
    let put = native("put", "(Ljava/lang/Object;)V")(
        &mut ctx,
        &[Value::Object(Some(queue)), Value::Object(Some(first))],
    )
    .unwrap();
    assert_eq!(put, None);
    assert_eq!(
        sq_consumer_collect(&mut ctx, key, root, ticket, false),
        Some(Value::Object(Some(first)))
    );

    // take -> a blocked producer.
    assert!(matches!(
        sq_produce(&mut ctx, queue, key, Value::Object(Some(second)), true),
        SqProduced::Filed(..)
    ));
    let taken = native("take", "()Ljava/lang/Object;")(&mut ctx, &[Value::Object(Some(queue))])
        .unwrap();
    assert_eq!(taken, Some(Value::Object(Some(second))));

    // poll(J, TimeUnit) -> a blocked producer.
    assert!(matches!(
        sq_produce(&mut ctx, queue, key, Value::Object(Some(third)), true),
        SqProduced::Filed(..)
    ));
    let polled = native("poll", POLL_TIMED)(
        &mut ctx,
        &[
            Value::Object(Some(queue)),
            Value::Long(50),
            Value::Object(Some(millis)),
        ],
    )
    .unwrap();
    assert_eq!(polled, Some(Value::Object(Some(third))));

    assert_eq!(ctx.global_root_count(), baseline, "nothing stays rooted");
    assert!(SQ_TABLE.peek(VM, |t| t.is_empty()).unwrap_or(true));
}

/// With no partner, an interrupted `take`, `put`, timed `poll` and timed
/// `offer` each leave the table, clear the interrupt status and throw
/// `InterruptedException` -- instead of spinning (`park` returns at once
/// while the status is set) -- and release every root they took.
#[test]
fn w19b_interrupted_waiters_withdraw_and_throw() {
    bounded("an interrupted SynchronousQueue waiter", || {
        const VM: usize = 0x19B0_0A03;
        let _teardown = Teardown(VM);
        let mut ctx = mock_on(VM);
        let queue = object(&mut ctx, 2);
        let item = object(&mut ctx, 1);
        let seconds = time_unit(&mut ctx, 3);
        let baseline = ctx.global_root_count();
        let calls: [(&str, &str, Vec<Value>); 4] = [
            (
                "take",
                "()Ljava/lang/Object;",
                vec![Value::Object(Some(queue))],
            ),
            (
                "put",
                "(Ljava/lang/Object;)V",
                vec![Value::Object(Some(queue)), Value::Object(Some(item))],
            ),
            (
                "poll",
                POLL_TIMED,
                vec![
                    Value::Object(Some(queue)),
                    Value::Long(60),
                    Value::Object(Some(seconds)),
                ],
            ),
            (
                "offer",
                OFFER_TIMED,
                vec![
                    Value::Object(Some(queue)),
                    Value::Object(Some(item)),
                    Value::Long(60),
                    Value::Object(Some(seconds)),
                ],
            ),
        ];
        for (name, desc, args) in calls {
            ctx.set_interrupted(true);
            let err = native(name, desc)(&mut ctx, &args)
                .expect_err("an interrupted waiter with no partner throws");
            assert!(
                is_interrupted_exception(&err),
                "{name}{desc} throws InterruptedException, got {err:?}"
            );
            assert!(
                !ctx.is_interrupted(false),
                "{name}{desc} clears the interrupt status before throwing"
            );
            assert_eq!(
                ctx.global_root_count(),
                baseline,
                "{name}{desc} releases every root"
            );
            assert!(
                SQ_TABLE.peek(VM, |t| t.is_empty()).unwrap_or(true),
                "{name}{desc} leaves no row behind"
            );
        }
    });
}

/// An interrupt that arrives after the partner already completed the
/// hand-off does not undo it: the consumer returns the element and the
/// producer reports success, both with the status left set.
#[test]
fn w19b_an_interrupt_after_the_hand_off_keeps_the_element() {
    const VM: usize = 0x19B0_0A04;
    let _teardown = Teardown(VM);
    let mut ctx = mock_on(VM);
    let queue = object(&mut ctx, 2);
    let item = object(&mut ctx, 1);
    let key = ctx.identity_hash_code(queue);
    let baseline = ctx.global_root_count();

    let SqConsumed::Filed(root, ticket) = sq_consume(&mut ctx, queue, key, true) else {
        panic!("filed");
    };
    assert!(matches!(
        sq_produce(&mut ctx, queue, key, Value::Object(Some(item)), false),
        SqProduced::Delivered
    ));
    ctx.set_interrupted(true);
    assert_eq!(
        sq_consumer_interrupted(&mut ctx, key, root, ticket).unwrap(),
        Some(Value::Object(Some(item)))
    );
    assert!(ctx.is_interrupted(false), "status left set");

    let SqProduced::Filed(root, ticket) =
        sq_produce(&mut ctx, queue, key, Value::Object(Some(item)), true)
    else {
        panic!("filed");
    };
    assert!(matches!(
        sq_consume(&mut ctx, queue, key, false),
        SqConsumed::Took(Value::Object(Some(o))) if o == item
    ));
    assert!(sq_producer_interrupted(&mut ctx, key, root, ticket).unwrap());
    assert!(ctx.is_interrupted(true), "status left set");
    assert_eq!(ctx.global_root_count(), baseline);
}

/// Timed waits read their `TimeUnit`; no unit keeps the old milliseconds
/// reading; a huge count saturates and still yields a deadline.
#[test]
fn w19b_timeouts_read_their_time_unit() {
    let mut ctx = MockNativeContext::new();
    let nanos = time_unit(&mut ctx, 0);
    let seconds = time_unit(&mut ctx, 3);
    let days = time_unit(&mut ctx, 6);
    let q = Value::Object(None);
    let at = |count: i64, unit: Value| vec![q, Value::Long(count), unit];
    assert_eq!(
        sq_timeout_nanos_in(&mut ctx, &at(2, Value::Object(Some(seconds))), 1),
        Some(2_000_000_000)
    );
    assert_eq!(
        sq_timeout_nanos_in(&mut ctx, &at(5, Value::Object(Some(nanos))), 1),
        Some(5)
    );
    assert_eq!(
        sq_timeout_nanos_in(&mut ctx, &at(7, Value::Object(None)), 1),
        Some(7_000_000),
        "a null unit keeps the milliseconds reading"
    );
    assert_eq!(
        sq_timeout_nanos_in(&mut ctx, &[q, Value::Long(7)], 1),
        Some(7_000_000)
    );
    assert_eq!(
        sq_timeout_nanos_in(&mut ctx, &at(0, Value::Object(Some(seconds))), 1),
        None
    );
    assert_eq!(
        sq_timeout_nanos_in(&mut ctx, &at(-3, Value::Object(Some(seconds))), 1),
        None
    );
    let huge = sq_timeout_nanos_in(&mut ctx, &at(i64::MAX, Value::Object(Some(days))), 1);
    assert_eq!(huge, Some(u64::MAX));
    assert!(sq_deadline(u64::MAX) > std::time::Instant::now());
}
