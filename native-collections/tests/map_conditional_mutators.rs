// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! `Map.remove(k,v)` / `Map.replace(k,v)` / `Map.replace(k,old,new)` — the
//! three conditional mutators.
//!
//! These are `java.util.Map` *default* methods that `HashMap` and `Hashtable`
//! override with bodies that walk the bucket array directly. They were never
//! registered as natives (only `ConcurrentHashMap` had them), so every call ran
//! the REAL JDK bytecode over a table whose nodes `native-collections`
//! allocates.
//!
//! On a `LinkedHashMap` that path is `HashMap.remove(k,v)` ->
//! `HashMap.removeNode` -> `LinkedHashMap.afterNodeRemoval`, whose first
//! statement is `(LinkedHashMap.Entry<K,V>) e` — and our nodes were
//! `java/util/LinkedHashMap$Node`, so it threw `ClassCastException:
//! java.util.LinkedHashMap$Node cannot be cast to java.util.LinkedHashMap$Entry`.
//! (That invented node class is fixed too — see `lhm_node_class_identity.rs` —
//! but these three still have to be natives regardless: the real bodies walk
//! the bucket array behind the native bookkeeping whether or not the cast
//! succeeds.)
//! Kafka's `MetadataLoader.removeAndClosePublisher` calls exactly
//! `publishers.remove(name, publisher)` on a `LinkedHashMap`, which failed
//! embedded-broker shutdown in Spring Boot's
//! `KafkaAutoConfigurationIntegrationTests`.
//!
//! The registration guard below is the cheap half. The behavioural tests are
//! the half that matters: a registration that dispatched to the wrong
//! semantics would satisfy the guard and still corrupt the map.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{boxed_int, build_registry, call, new_hashmap, new_linked_hashmap, MockCtx};
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::{ObjectRef, Value};

const HM: &str = "java/util/HashMap";
const LHM: &str = "java/util/LinkedHashMap";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const GET: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
const SIZE: &str = "()I";
const REMOVE_KV: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Z";
const REPLACE_KV: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const REPLACE_KOV: &str = "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z";

fn put(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    class: &str,
    m: ObjectRef,
    k: Value,
    v: Value,
) {
    call(reg, ctx, class, "put", PUT, &[Value::Object(Some(m)), k, v]).unwrap();
}

fn get(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    class: &str,
    m: ObjectRef,
    k: Value,
) -> Value {
    call(reg, ctx, class, "get", GET, &[Value::Object(Some(m)), k])
        .unwrap()
        .unwrap_or(Value::Object(None))
}

fn size(reg: &NativeMethodRegistry, ctx: &mut MockCtx, class: &str, m: ObjectRef) -> i32 {
    match call(reg, ctx, class, "size", SIZE, &[Value::Object(Some(m))]).unwrap() {
        Some(Value::Int(n)) => n,
        other => panic!("size returned {other:?}"),
    }
}

/// `get(k)` unboxed — one statement so the `&mut ctx` for the call and the
/// `&ctx` for the field read don't overlap.
fn get_int(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    class: &str,
    m: ObjectRef,
    k: Value,
) -> Option<i32> {
    let v = get(reg, ctx, class, m, k);
    int_of(ctx, v)
}

fn int_of(ctx: &MockCtx, v: Value) -> Option<i32> {
    match v {
        Value::Object(Some(o)) => match ctx.get_field(o, 0) {
            Value::Int(i) => Some(i),
            _ => None,
        },
        Value::Int(i) => Some(i),
        _ => None,
    }
}

/// Every map family whose JDK class overrides the three conditional mutators
/// must have all three registered. `ConcurrentHashMap` already did; the other
/// four are the regression.
#[test]
fn conditional_mutators_are_registered_for_every_bucket_walking_map() {
    let reg = build_registry();
    for class in [
        "java/util/HashMap",
        "java/util/LinkedHashMap",
        "java/util/Hashtable",
        "java/util/Properties",
        "java/util/concurrent/ConcurrentHashMap",
    ] {
        assert!(
            reg.find(class, "remove", REMOVE_KV).is_some(),
            "{class}.remove(Object,Object) must be native — otherwise the real \
             JDK body walks our bucket array"
        );
        assert!(
            reg.find(class, "replace", REPLACE_KV).is_some(),
            "{class}.replace(Object,Object) must be native"
        );
        assert!(
            reg.find(class, "replace", REPLACE_KOV).is_some(),
            "{class}.replace(Object,Object,Object) must be native"
        );
    }
}

/// The shape Kafka's `MetadataLoader` uses: `publishers.remove(name, publisher)`
/// on a `LinkedHashMap`, where the value genuinely matches.
#[test]
fn lhm_remove_kv_removes_when_the_value_matches() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, false);

    let k1 = boxed_int(&mut ctx, 1);
    let v1 = boxed_int(&mut ctx, 10);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = boxed_int(&mut ctx, 20);
    put(&reg, &mut ctx, LHM, lhm, k1, v1);
    put(&reg, &mut ctx, LHM, lhm, k2, v2);

    let expected = boxed_int(&mut ctx, 10);
    let removed = call(
        &reg,
        &mut ctx,
        LHM,
        "remove",
        REMOVE_KV,
        &[Value::Object(Some(lhm)), k1, expected],
    )
    .unwrap();
    assert_eq!(removed, Some(Value::Int(1)), "matching value must remove");

    assert_eq!(size(&reg, &mut ctx, LHM, lhm), 1, "size must drop to 1");
    assert!(
        matches!(get(&reg, &mut ctx, LHM, lhm, k1), Value::Object(None)),
        "k1 must be gone"
    );
    assert_eq!(
        get_int(&reg, &mut ctx, LHM, lhm, k2),
        Some(20),
        "k2 must be untouched"
    );
}

#[test]
fn lhm_remove_kv_keeps_the_entry_when_the_value_differs() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, false);

    let k1 = boxed_int(&mut ctx, 1);
    let v1 = boxed_int(&mut ctx, 10);
    put(&reg, &mut ctx, LHM, lhm, k1, v1);

    let wrong = boxed_int(&mut ctx, 99);
    let removed = call(
        &reg,
        &mut ctx,
        LHM,
        "remove",
        REMOVE_KV,
        &[Value::Object(Some(lhm)), k1, wrong],
    )
    .unwrap();
    assert_eq!(
        removed,
        Some(Value::Int(0)),
        "mismatched value must not remove"
    );
    assert_eq!(size(&reg, &mut ctx, LHM, lhm), 1);
    assert_eq!(get_int(&reg, &mut ctx, LHM, lhm, k1), Some(10));
}

#[test]
fn hashmap_remove_kv_on_an_absent_key_is_false_and_inserts_nothing() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    let k = boxed_int(&mut ctx, 7);
    let v = boxed_int(&mut ctx, 70);
    let removed = call(
        &reg,
        &mut ctx,
        HM,
        "remove",
        REMOVE_KV,
        &[Value::Object(Some(hm)), k, v],
    )
    .unwrap();
    assert_eq!(removed, Some(Value::Int(0)));
    assert_eq!(size(&reg, &mut ctx, HM, hm), 0, "must not have inserted");
}

#[test]
fn replace_returns_the_old_value_and_updates_only_a_present_key() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);

    let k = boxed_int(&mut ctx, 1);
    let v = boxed_int(&mut ctx, 10);
    put(&reg, &mut ctx, HM, hm, k, v);

    let new_v = boxed_int(&mut ctx, 11);
    let old = call(
        &reg,
        &mut ctx,
        HM,
        "replace",
        REPLACE_KV,
        &[Value::Object(Some(hm)), k, new_v],
    )
    .unwrap()
    .unwrap_or(Value::Object(None));
    assert_eq!(int_of(&ctx, old), Some(10), "replace returns the old value");
    assert_eq!(get_int(&reg, &mut ctx, HM, hm, k), Some(11));

    // Absent key: null, and no insertion.
    let absent = boxed_int(&mut ctx, 42);
    let val = boxed_int(&mut ctx, 420);
    let out = call(
        &reg,
        &mut ctx,
        HM,
        "replace",
        REPLACE_KV,
        &[Value::Object(Some(hm)), absent, val],
    )
    .unwrap()
    .unwrap_or(Value::Object(None));
    assert!(
        matches!(out, Value::Object(None)),
        "replace on an absent key returns null, got {out:?}"
    );
    assert_eq!(size(&reg, &mut ctx, HM, hm), 1, "must not have inserted");
}

#[test]
fn replace_compare_and_set_honours_the_expected_old_value() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, false);

    let k = boxed_int(&mut ctx, 1);
    let v = boxed_int(&mut ctx, 10);
    put(&reg, &mut ctx, LHM, lhm, k, v);

    // Wrong expectation → no change.
    let wrong = boxed_int(&mut ctx, 99);
    let new_v = boxed_int(&mut ctx, 11);
    let out = call(
        &reg,
        &mut ctx,
        LHM,
        "replace",
        REPLACE_KOV,
        &[Value::Object(Some(lhm)), k, wrong, new_v],
    )
    .unwrap();
    assert_eq!(out, Some(Value::Int(0)));
    assert_eq!(get_int(&reg, &mut ctx, LHM, lhm, k), Some(10));

    // Right expectation → swapped.
    let right = boxed_int(&mut ctx, 10);
    let new_v = boxed_int(&mut ctx, 11);
    let out = call(
        &reg,
        &mut ctx,
        LHM,
        "replace",
        REPLACE_KOV,
        &[Value::Object(Some(lhm)), k, right, new_v],
    )
    .unwrap();
    assert_eq!(out, Some(Value::Int(1)));
    assert_eq!(get_int(&reg, &mut ctx, LHM, lhm, k), Some(11));
    assert_eq!(size(&reg, &mut ctx, LHM, lhm), 1);
}
