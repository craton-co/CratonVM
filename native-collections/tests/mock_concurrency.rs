// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! Focused coverage for synthetic concurrency natives.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{boxed_int, build_registry, call, MockCtx};
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

const CF: &str = "java/util/concurrent/CompletableFuture";
const COWAL: &str = "java/util/concurrent/CopyOnWriteArrayList";
const PBQ: &str = "java/util/concurrent/PriorityBlockingQueue";

fn alloc_obj(ctx: &mut MockCtx, class_name: &str, fields: usize) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class_name).unwrap();
    ctx.alloc_object(cid, fields)
}

fn new_cf(ctx: &mut MockCtx, done: i32, result: Value) -> ObjectRef {
    let cf = alloc_obj(ctx, CF, 4);
    ctx.set_field(cf, 0, result);
    ctx.set_field(cf, 1, Value::Int(done));
    cf
}

fn cf_array(ctx: &mut MockCtx, cfs: &[ObjectRef]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Reference, cfs.len());
    for (i, cf) in cfs.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Object(Some(*cf)));
    }
    arr
}

fn new_priority_blocking_queue(reg: &NativeMethodRegistry, ctx: &mut MockCtx) -> ObjectRef {
    let queue = alloc_obj(ctx, PBQ, 2);
    call(
        reg,
        ctx,
        PBQ,
        "<init>",
        "()V",
        &[Value::Object(Some(queue))],
    )
    .unwrap();
    queue
}

fn object_result(result: MethodCallResult) -> ObjectRef {
    match result.unwrap().unwrap() {
        Value::Object(Some(obj)) => obj,
        other => panic!("expected object result, got {other:?}"),
    }
}

fn assert_npe(result: MethodCallResult) {
    assert!(
        matches!(
            &result,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { .. }
            )))
        ),
        "expected NullPointerException, got {result:?}"
    );
}

fn callback_count(ctx: &MockCtx, method: &str) -> usize {
    ctx.invoke_virtual_log()
        .iter()
        .filter(|(_, m, _, _)| m == method)
        .count()
}

fn is_completed_exceptionally(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    cf: ObjectRef,
) -> bool {
    matches!(
        call(
            reg,
            ctx,
            CF,
            "isCompletedExceptionally",
            "()Z",
            &[Value::Object(Some(cf))]
        )
        .unwrap(),
        Some(Value::Int(1))
    )
}

fn alt_result_exception(ctx: &MockCtx, cf: ObjectRef) -> Value {
    match ctx.get_field(cf, 0) {
        Value::Object(Some(alt)) => ctx.get_field(alt, 0),
        other => panic!("expected AltResult in result slot, got {other:?}"),
    }
}

#[test]
fn cf_pending_dependents_do_not_eagerly_invoke_callbacks_with_null() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let pending = new_cf(&mut ctx, 0, Value::Object(None));

    let func = alloc_obj(&mut ctx, "test/Function", 0);
    let then_apply = object_result(call(
        &reg,
        &mut ctx,
        CF,
        "thenApply",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(Some(pending)), Value::Object(Some(func))],
    ));
    assert_eq!(ctx.get_field(then_apply, 1), Value::Int(0));
    assert_eq!(callback_count(&ctx, "apply"), 0);

    ctx.clear_invoke_virtual_log();
    let consumer = alloc_obj(&mut ctx, "test/BiConsumer", 0);
    let when_complete = object_result(call(
        &reg,
        &mut ctx,
        CF,
        "whenComplete",
        "(Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(Some(pending)), Value::Object(Some(consumer))],
    ));
    assert_eq!(ctx.get_field(when_complete, 1), Value::Int(0));
    assert_eq!(callback_count(&ctx, "accept"), 0);
}

#[test]
fn cf_static_combinators_do_not_complete_eagerly_from_pending_inputs() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let pending = new_cf(&mut ctx, 0, Value::Object(None));
    let inputs = cf_array(&mut ctx, &[pending]);

    let all = object_result(call(
        &reg,
        &mut ctx,
        CF,
        "allOf",
        "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(Some(inputs))],
    ));
    assert_eq!(ctx.get_field(all, 1), Value::Int(0));

    let any = object_result(call(
        &reg,
        &mut ctx,
        CF,
        "anyOf",
        "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(Some(inputs))],
    ));
    assert_eq!(ctx.get_field(any, 1), Value::Int(0));
}

#[test]
fn cf_exceptional_then_apply_propagates_without_invoking_function() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let throwable = alloc_obj(&mut ctx, "java/lang/RuntimeException", 2);
    let failed = new_cf(&mut ctx, 2, Value::Object(Some(throwable)));
    let func = alloc_obj(&mut ctx, "test/Function", 0);

    let dependent = object_result(call(
        &reg,
        &mut ctx,
        CF,
        "thenApply",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(Some(failed)), Value::Object(Some(func))],
    ));

    assert_eq!(callback_count(&ctx, "apply"), 0);
    assert!(is_completed_exceptionally(&reg, &mut ctx, dependent));
    assert_eq!(
        alt_result_exception(&ctx, dependent),
        Value::Object(Some(throwable))
    );
}

#[test]
fn cf_callback_failure_completes_dependent_exceptionally() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let value = boxed_int(&mut ctx, 7);
    let source = new_cf(&mut ctx, 1, value);
    let consumer = alloc_obj(&mut ctx, "test/BiConsumer", 0);
    let thrown = alloc_obj(&mut ctx, "java/lang/IllegalStateException", 2);
    ctx.set_invoke_virtual_result(Err(MethodCallFailed::ExceptionThrown(thrown)));

    let dependent = object_result(call(
        &reg,
        &mut ctx,
        CF,
        "whenComplete",
        "(Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(Some(source)), Value::Object(Some(consumer))],
    ));

    assert_eq!(callback_count(&ctx, "accept"), 1);
    assert!(is_completed_exceptionally(&reg, &mut ctx, dependent));
    assert_eq!(
        alt_result_exception(&ctx, dependent),
        Value::Object(Some(thrown))
    );
}

#[test]
fn cf_null_callbacks_throw_null_pointer_exception() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let value = boxed_int(&mut ctx, 1);
    let source = new_cf(&mut ctx, 1, value);

    for (method, desc) in [
        (
            "thenApply",
            "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        ),
        (
            "thenAccept",
            "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;",
        ),
        (
            "thenRun",
            "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;",
        ),
        (
            "exceptionally",
            "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        ),
        (
            "handle",
            "(Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;",
        ),
        (
            "whenComplete",
            "(Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;",
        ),
    ] {
        assert_npe(call(
            &reg,
            &mut ctx,
            CF,
            method,
            desc,
            &[Value::Object(Some(source)), Value::Object(None)],
        ));
    }

    let other_value = boxed_int(&mut ctx, 2);
    let other = new_cf(&mut ctx, 1, other_value);
    assert_npe(call(
        &reg,
        &mut ctx,
        CF,
        "thenCombine",
        "(Ljava/util/concurrent/CompletionStage;Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;",
        &[
            Value::Object(Some(source)),
            Value::Object(Some(other)),
            Value::Object(None),
        ],
    ));
}

#[test]
fn cowal_add_if_absent_updates_snapshot_without_split_virtual_calls() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let list = alloc_obj(&mut ctx, COWAL, 2);
    let elem = boxed_int(&mut ctx, 11);

    let added = call(
        &reg,
        &mut ctx,
        COWAL,
        "addIfAbsent",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), elem],
    )
    .unwrap();
    assert_eq!(added, Some(Value::Int(1)));
    assert_eq!(ctx.get_field(list, 1), Value::Int(1));
    let arr = match ctx.get_field(list, 0) {
        Value::Object(Some(a)) => a,
        other => panic!("CopyOnWriteArrayList array was {:?}", other),
    };
    assert_eq!(ctx.array_length(arr), 1);
    assert_eq!(ctx.get_array_element(arr, 0), elem);
    assert!(
        ctx.invoke_virtual_log()
            .iter()
            .all(|(_, method, _, _)| method != "contains" && method != "add"),
        "addIfAbsent must not split atomicity through virtual contains/add"
    );

    ctx.clear_invoke_virtual_log();
    let duplicate = call(
        &reg,
        &mut ctx,
        COWAL,
        "addIfAbsent",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), elem],
    )
    .unwrap();
    assert_eq!(duplicate, Some(Value::Int(0)));
    assert_eq!(ctx.get_field(list, 1), Value::Int(1));
    assert!(
        ctx.invoke_virtual_log()
            .iter()
            .all(|(_, method, _, _)| method != "contains" && method != "add"),
        "duplicate addIfAbsent must rescan under the native lock"
    );
}

#[test]
fn priority_blocking_queue_rejects_null_offer_and_put() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let queue = new_priority_blocking_queue(&reg, &mut ctx);

    assert_npe(call(
        &reg,
        &mut ctx,
        PBQ,
        "offer",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(queue)), Value::Object(None)],
    ));
    assert_npe(call(
        &reg,
        &mut ctx,
        PBQ,
        "put",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(queue)), Value::Object(None)],
    ));
    assert_eq!(ctx.get_field(queue, 1), Value::Int(0));
}

#[test]
fn priority_blocking_queue_take_removes_available_head() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let queue = new_priority_blocking_queue(&reg, &mut ctx);

    for value in [
        boxed_int(&mut ctx, 30),
        boxed_int(&mut ctx, 10),
        boxed_int(&mut ctx, 20),
    ] {
        let offered = call(
            &reg,
            &mut ctx,
            PBQ,
            "offer",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(queue)), value],
        )
        .unwrap();
        assert_eq!(offered, Some(Value::Int(1)));
    }

    let taken = call(
        &reg,
        &mut ctx,
        PBQ,
        "take",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(queue))],
    )
    .unwrap();
    match taken {
        Some(Value::Object(Some(obj))) => assert_eq!(ctx.get_field(obj, 0), Value::Int(10)),
        other => panic!("take returned {:?}", other),
    }
    assert_eq!(ctx.get_field(queue, 1), Value::Int(2));
}

fn pbq_offer(reg: &NativeMethodRegistry, ctx: &mut MockCtx, queue: ObjectRef, v: Value) {
    let offered = call(
        reg,
        ctx,
        PBQ,
        "offer",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(queue)), v],
    )
    .unwrap();
    assert_eq!(offered, Some(Value::Int(1)));
}

fn pbq_poll_int(reg: &NativeMethodRegistry, ctx: &mut MockCtx, queue: ObjectRef) -> Value {
    let head = object_result(call(
        reg,
        ctx,
        PBQ,
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(queue))],
    ));
    ctx.get_field(head, 0)
}

/// Interpreter round i3 wave 79, lane L1: the stand-in orders by the queue's
/// own `comparator` field (the JDK's two-argument constructor sets it).
#[test]
fn i79_priority_blocking_queue_orders_by_its_comparator() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let queue = new_priority_blocking_queue(&reg, &mut ctx);
    // One of the crate's factory comparators: three fields, the tag in slot 0;
    // tag 2 is `CMP_TAG_REVERSE_ORDER` (`Comparator.reverseOrder()`).
    let cmp = alloc_obj(&mut ctx, "test/ReverseOrderComparator", 3);
    ctx.set_field(cmp, 0, Value::Int(2));
    ctx.set_field_by_name_for_test(queue, "comparator", Value::Object(Some(cmp)));
    for v in [1, 3, 2] {
        let b = boxed_int(&mut ctx, v);
        pbq_offer(&reg, &mut ctx, queue, b);
    }
    let polled: Vec<Value> = (0..3)
        .map(|_| pbq_poll_int(&reg, &mut ctx, queue))
        .collect();
    assert_eq!(polled, vec![Value::Int(3), Value::Int(2), Value::Int(1)]);
}

/// No `comparator` (null, or the synthetic layout): natural order, as before.
#[test]
fn i79_priority_blocking_queue_without_a_comparator_is_natural_order() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let queue = new_priority_blocking_queue(&reg, &mut ctx);
    for v in [1, 3, 2] {
        let b = boxed_int(&mut ctx, v);
        pbq_offer(&reg, &mut ctx, queue, b);
    }
    let polled: Vec<Value> = (0..3)
        .map(|_| pbq_poll_int(&reg, &mut ctx, queue))
        .collect();
    assert_eq!(polled, vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
}

fn assert_interrupted(r: MethodCallResult) {
    match r {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::InterruptedException,
        ))) => {}
        other => panic!("expected InterruptedException, got {other:?}"),
    }
}

/// Interpreter round i3 wave 79, lane L1: a waiter's interrupt ends `take()`
/// and `poll(long, TimeUnit)` with `InterruptedException` (the wait's error
/// used to be discarded), and a pending interrupt raises at entry
/// (`lockInterruptibly`) even with an element ready.
#[test]
fn i79_priority_blocking_queue_waits_answer_an_interrupt() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let queue = new_priority_blocking_queue(&reg, &mut ctx);
    let unit = alloc_obj(&mut ctx, "java/util/concurrent/TimeUnit", 1);
    ctx.set_field(unit, 0, Value::Int(3)); // SECONDS

    ctx.set_monitor_wait_interrupts(true);
    assert_interrupted(call(
        &reg,
        &mut ctx,
        PBQ,
        "take",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(queue))],
    ));
    assert_interrupted(call(
        &reg,
        &mut ctx,
        PBQ,
        "poll",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        &[
            Value::Object(Some(queue)),
            Value::Long(60),
            Value::Object(Some(unit)),
        ],
    ));
    ctx.set_monitor_wait_interrupts(false);

    let b = boxed_int(&mut ctx, 7);
    pbq_offer(&reg, &mut ctx, queue, b);
    ctx.set_interrupted(true);
    assert_interrupted(call(
        &reg,
        &mut ctx,
        PBQ,
        "take",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(queue))],
    ));
    assert!(!ctx.is_interrupted(false), "the interrupt is consumed");
    ctx.set_interrupted(true);
    assert_interrupted(call(
        &reg,
        &mut ctx,
        PBQ,
        "poll",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        &[
            Value::Object(Some(queue)),
            Value::Long(60),
            Value::Object(Some(unit)),
        ],
    ));
    assert_eq!(
        ctx.get_field(queue, 1),
        Value::Int(1),
        "the element stays queued"
    );
}

// The two `stamped_lock_*` tests that lived here were deleted on 2026-07-30.
//
// They exercised this crate's `StampedLock` natives, which were DISABLED on
// 2026-07-28 because they silently destroyed mutual exclusion, and they
// asserted precisely the two defects that fix removed: `writeLock()` returning
// stamp 0 after giving up (that is `tryWriteLock`'s failure contract, not
// `writeLock`'s), and the lock word living in field 0 as a `Value::Int` (in
// real-JDK mode field 0 of a real `StampedLock` is `state`, a `long`).
//
// Reinstating them would mean reinstating the bug. The surviving
// implementation is `native-builtins`' `parking_lot`-backed one, and its
// registration surface is gated by
// `native-builtins/tests/registry_contracts.rs`.

/// nio20 wave 5 (L1): on a REAL-layout `CompletableFuture` (slot 1 is the
/// `stack` reference, null here: no dependents) `complete` is the JDK's
/// `RESULT` CAS: the first completer stores its value and answers `true`, a
/// second one answers `false` and leaves the first value in place. The native
/// used to read-then-store, so both racers could win.
#[test]
fn cf_complete_on_a_real_layout_future_is_first_wins() {
    let reg = build_registry();
    let mut ctx = MockCtx::default();
    let cf = alloc_obj(&mut ctx, CF, 2);
    ctx.set_field(cf, 0, Value::Object(None));
    ctx.set_field(cf, 1, Value::Object(None));
    let first = alloc_obj(&mut ctx, "java/lang/Object", 0);
    let second = alloc_obj(&mut ctx, "java/lang/Object", 0);
    let won = call(
        &reg,
        &mut ctx,
        CF,
        "complete",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(cf)), Value::Object(Some(first))],
    )
    .unwrap();
    assert_eq!(won, Some(Value::Int(1)));
    assert_eq!(ctx.get_field(cf, 0), Value::Object(Some(first)));
    let lost = call(
        &reg,
        &mut ctx,
        CF,
        "complete",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(cf)), Value::Object(Some(second))],
    )
    .unwrap();
    assert_eq!(lost, Some(Value::Int(0)));
    assert_eq!(ctx.get_field(cf, 0), Value::Object(Some(first)));
    // No waiter on `stack`: the `postComplete` callback is skipped.
    assert_eq!(callback_count(&ctx, "postComplete"), 0);
}
