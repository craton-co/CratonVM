// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! Regression coverage for native roots held across Java callbacks.

mod common;

use common::{
    boxed_int, build_registry, call, new_arraylist, new_linked_hashmap, new_treemap, MockCtx,
};
use cratonvm_native_api::NativeContext;
use cratonvm_types::Value;

const STREAM: &str = "java/util/stream/Stream";
const AL: &str = "java/util/ArrayList";
const LHM: &str = "java/util/LinkedHashMap";
const TM: &str = "java/util/TreeMap";

fn object_value(ctx: &mut MockCtx, class_id: u32) -> Value {
    Value::Object(Some(ctx.alloc_object_simple(class_id)))
}

#[test]
fn stream_for_each_reads_forwarded_consumer_and_elements() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();

    let e1 = object_value(&mut ctx, 101);
    let e2 = object_value(&mut ctx, 102);
    let consumer = ctx.alloc_object_simple(200);
    let stream = match cratonvm_native_collections::make_stream_from_elements(&mut ctx, &[e1, e2])
        .unwrap()
    {
        Some(Value::Object(Some(s))) => s,
        other => panic!("expected stream object, got {other:?}"),
    };

    ctx.set_relocate_pins_on_invoke(true);
    call(
        &reg,
        &mut ctx,
        STREAM,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        &[Value::Object(Some(stream)), Value::Object(Some(consumer))],
    )
    .unwrap();

    let log = ctx.invoke_virtual_log();
    assert_eq!(log.len(), 2, "forEach should invoke the consumer twice");
    assert_eq!(log[0].0, consumer.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        consumer.as_ptr() as usize,
        "consumer must be re-read from its native pin after callback GC"
    );
    assert_ne!(
        log[1].3[0], e2,
        "second element must be re-read from its native pin after callback GC"
    );
}

#[test]
fn arraylist_remove_if_reads_forwarded_predicate_and_elements() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let al = new_arraylist(&reg, &mut ctx);
    let e1 = object_value(&mut ctx, 301);
    let e2 = object_value(&mut ctx, 302);
    let predicate = ctx.alloc_object_simple(400);

    call(
        &reg,
        &mut ctx,
        AL,
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(al)), e1],
    )
    .unwrap();
    call(
        &reg,
        &mut ctx,
        AL,
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(al)), e2],
    )
    .unwrap();

    ctx.set_invoke_virtual_result(Ok(Some(Value::Int(1))));
    ctx.set_relocate_pins_on_invoke(true);
    let removed = call(
        &reg,
        &mut ctx,
        AL,
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
        &[Value::Object(Some(al)), Value::Object(Some(predicate))],
    )
    .unwrap();

    assert_eq!(removed, Some(Value::Int(1)));
    let log = ctx.invoke_virtual_log();
    assert_eq!(log.len(), 2, "removeIf should test both elements");
    assert_eq!(log[0].0, predicate.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        predicate.as_ptr() as usize,
        "predicate must be re-read from its native pin after callback GC"
    );
    assert_ne!(
        log[1].3[0], e2,
        "second element must be re-read from its native pin after callback GC"
    );
}

#[test]
fn linked_hashmap_for_each_reads_forwarded_consumer() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, false);
    let k1 = boxed_int(&mut ctx, 1);
    let v1 = object_value(&mut ctx, 501);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = object_value(&mut ctx, 502);
    let consumer = ctx.alloc_object_simple(503);

    for (k, v) in [(k1, v1), (k2, v2)] {
        call(
            &reg,
            &mut ctx,
            LHM,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(lhm)), k, v],
        )
        .unwrap();
    }

    ctx.set_relocate_pins_on_invoke(true);
    call(
        &reg,
        &mut ctx,
        LHM,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        &[Value::Object(Some(lhm)), Value::Object(Some(consumer))],
    )
    .unwrap();

    let log = ctx.invoke_virtual_log();
    assert_eq!(
        log.len(),
        2,
        "LinkedHashMap.forEach should visit two entries"
    );
    assert_eq!(log[0].0, consumer.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        consumer.as_ptr() as usize,
        "BiConsumer must be re-read from its native pin after callback GC"
    );
}

#[test]
fn treemap_for_each_reads_forwarded_action_and_pairs() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let tm = new_treemap(&reg, &mut ctx);
    let k1 = boxed_int(&mut ctx, 1);
    let v1 = object_value(&mut ctx, 601);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = object_value(&mut ctx, 602);
    let action = ctx.alloc_object_simple(603);

    for (k, v) in [(k1, v1), (k2, v2)] {
        call(
            &reg,
            &mut ctx,
            TM,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(tm)), k, v],
        )
        .unwrap();
    }

    ctx.set_relocate_pins_on_invoke(true);
    call(
        &reg,
        &mut ctx,
        TM,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        &[Value::Object(Some(tm)), Value::Object(Some(action))],
    )
    .unwrap();

    let log = ctx.invoke_virtual_log();
    assert_eq!(log.len(), 2, "TreeMap.forEach should visit two entries");
    assert_eq!(log[0].0, action.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        action.as_ptr() as usize,
        "BiConsumer must be re-read from its native pin after callback GC"
    );
    assert_ne!(
        log[1].3[0], k2,
        "second key must be re-read from its native pin after callback GC"
    );
    assert_ne!(
        log[1].3[1], v2,
        "second value must be re-read from its native pin after callback GC"
    );
}
