// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.

//! Regression coverage for native roots held across Java callbacks.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};

use common::{
    boxed_int, build_registry, call, new_arraylist, new_concurrent_hashmap, new_hashmap,
    new_linked_hashmap, new_treemap, MockCtx,
};
use cratonvm_native_api::NativeContext;
use cratonvm_types::Value;

const STREAM: &str = "java/util/stream/Stream";
const AL: &str = "java/util/ArrayList";
const HM: &str = "java/util/HashMap";
const CHM: &str = "java/util/concurrent/ConcurrentHashMap";
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

#[test]
fn hashmap_replace_all_reads_forwarded_function_and_entries() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);
    let k1 = boxed_int(&mut ctx, 1);
    let v1 = object_value(&mut ctx, 701);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = object_value(&mut ctx, 702);
    let function = ctx.alloc_object_simple(703);

    for (k, v) in [(k1, v1), (k2, v2)] {
        call(
            &reg,
            &mut ctx,
            HM,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(hm)), k, v],
        )
        .unwrap();
    }

    ctx.set_relocate_pins_on_invoke(true);
    call(
        &reg,
        &mut ctx,
        HM,
        "replaceAll",
        "(Ljava/util/function/BiFunction;)V",
        &[Value::Object(Some(hm)), Value::Object(Some(function))],
    )
    .unwrap();

    let log = ctx.invoke_virtual_log();
    assert_eq!(log.len(), 2, "HashMap.replaceAll should visit two entries");
    assert_eq!(log[0].0, function.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        function.as_ptr() as usize,
        "BiFunction must be re-read from its native pin after callback GC"
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

#[test]
fn concurrent_hashmap_for_each_key_reads_forwarded_action_and_keys() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let chm = new_concurrent_hashmap(&reg, &mut ctx);
    let k1 = boxed_int(&mut ctx, 1);
    let v1 = object_value(&mut ctx, 801);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = object_value(&mut ctx, 802);
    let action = ctx.alloc_object_simple(803);

    for (k, v) in [(k1, v1), (k2, v2)] {
        call(
            &reg,
            &mut ctx,
            CHM,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(chm)), k, v],
        )
        .unwrap();
    }

    ctx.set_relocate_pins_on_invoke(true);
    call(
        &reg,
        &mut ctx,
        CHM,
        "forEachKey",
        "(JLjava/util/function/Consumer;)V",
        &[
            Value::Object(Some(chm)),
            Value::Long(1),
            Value::Object(Some(action)),
        ],
    )
    .unwrap();

    let log = ctx.invoke_virtual_log();
    assert_eq!(
        log.len(),
        2,
        "ConcurrentHashMap.forEachKey should visit two keys"
    );
    assert_eq!(log[0].0, action.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        action.as_ptr() as usize,
        "Consumer must be re-read from its native pin after callback GC"
    );
    assert!(
        log[1].3[0] != k1 && log[1].3[0] != k2,
        "second key must be re-read from its native pin after callback GC"
    );
}

#[test]
fn concurrent_hashmap_search_reads_forwarded_function_and_entries() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let chm = new_concurrent_hashmap(&reg, &mut ctx);
    let k1 = boxed_int(&mut ctx, 1);
    let v1 = object_value(&mut ctx, 901);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = object_value(&mut ctx, 902);
    let function = ctx.alloc_object_simple(903);

    for (k, v) in [(k1, v1), (k2, v2)] {
        call(
            &reg,
            &mut ctx,
            CHM,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(chm)), k, v],
        )
        .unwrap();
    }

    ctx.set_relocate_pins_on_invoke(true);
    let result = call(
        &reg,
        &mut ctx,
        CHM,
        "search",
        "(JLjava/util/function/BiFunction;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Long(1),
            Value::Object(Some(function)),
        ],
    )
    .unwrap();

    assert_eq!(result, Some(Value::Object(None)));
    let log = ctx.invoke_virtual_log();
    assert_eq!(
        log.len(),
        2,
        "ConcurrentHashMap.search should visit two entries"
    );
    assert_eq!(log[0].0, function.as_ptr() as usize);
    assert_ne!(
        log[1].0,
        function.as_ptr() as usize,
        "BiFunction must be re-read from its native pin after callback GC"
    );
    assert!(
        log[1].3[0] != k1 && log[1].3[0] != k2,
        "second key must be re-read from its native pin after callback GC"
    );
    assert!(
        log[1].3[1] != v1 && log[1].3[1] != v2,
        "second value must be re-read from its native pin after callback GC"
    );
}
