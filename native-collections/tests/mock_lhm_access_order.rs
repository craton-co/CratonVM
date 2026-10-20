// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! Behavioural coverage for access-order `LinkedHashMap`.
//!
//! These tests exercise the C25 access-order fix at
//! `native_lhm_put` (review §1.1 row 4): in an access-order LHM, every
//! `get` and every `put` of an existing key must move the entry to the
//! tail of the insertion-order list so iteration reflects LRU semantics.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{boxed_int, build_registry, call, new_linked_hashmap, MockCtx};
use cratonvm_native_api::NativeContext;
use cratonvm_types::{ClassId, Value};

const LHM: &str = "java/util/LinkedHashMap";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const GET: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
const FOR_EACH: &str = "(Ljava/util/function/BiConsumer;)V";

/// Drive `LHM.forEach(consumer)` and return the keys in the order the
/// consumer's `accept(k, v)` was invoked. The consumer is a synthetic
/// `java.util.function.BiConsumer` placeholder; the mock's
/// `invoke_virtual` log records each call so we can recover the
/// iteration order without standing up a real lambda.
fn iter_keys_via_for_each(
    reg: &cratonvm_native_api::NativeMethodRegistry,
    ctx: &mut MockCtx,
    lhm: cratonvm_types::ObjectRef,
) -> Vec<Value> {
    let consumer_cid = ctx
        .ensure_class_initialized("java/util/function/BiConsumer")
        .unwrap();
    let consumer = ctx.alloc_object(consumer_cid, 1);
    ctx.clear_invoke_virtual_log();
    call(
        reg,
        ctx,
        LHM,
        "forEach",
        FOR_EACH,
        &[Value::Object(Some(lhm)), Value::Object(Some(consumer))],
    )
    .unwrap();
    ctx.invoke_virtual_log()
        .into_iter()
        .filter(|(_, m, _, _)| m == "accept")
        .map(|(_, _, _, a)| a[0])
        .collect()
}

#[test]
fn access_order_get_moves_entry_to_tail() {
    // Setup: put k1, put k2, get(k1). Iteration order should be k2, k1.
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, /*access_order=*/ true);

    let k1 = boxed_int(&mut ctx, 1);
    let v1 = boxed_int(&mut ctx, 10);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = boxed_int(&mut ctx, 20);

    call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), k1, v1],
    )
    .unwrap();
    call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), k2, v2],
    )
    .unwrap();

    // Touch k1 → it must move to the tail in access order.
    let got = call(
        &reg,
        &mut ctx,
        LHM,
        "get",
        GET,
        &[Value::Object(Some(lhm)), k1],
    )
    .unwrap();
    assert_eq!(got, Some(v1));

    let order = iter_keys_via_for_each(&reg, &mut ctx, lhm);
    assert_eq!(
        order,
        vec![k2, k1],
        "access-order LHM: after get(k1), iteration should be k2 (LRU), k1 (MRU)"
    );
}

#[test]
fn insertion_order_get_does_not_reorder() {
    // The same sequence on a default-order LHM must keep iteration as
    // k1, k2 regardless of how many gets land on k1.
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, /*access_order=*/ false);

    let k1 = boxed_int(&mut ctx, 1);
    let v1 = boxed_int(&mut ctx, 10);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = boxed_int(&mut ctx, 20);

    call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), k1, v1],
    )
    .unwrap();
    call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), k2, v2],
    )
    .unwrap();
    call(
        &reg,
        &mut ctx,
        LHM,
        "get",
        GET,
        &[Value::Object(Some(lhm)), k1],
    )
    .unwrap();

    let order = iter_keys_via_for_each(&reg, &mut ctx, lhm);
    assert_eq!(
        order,
        vec![k1, k2],
        "insertion-order LHM: get(k1) must NOT reorder"
    );
}

#[test]
fn insertion_order_beats_bucket_order() {
    // The property Jackson's `ObjectNode._children` (a default LinkedHashMap)
    // relies on, and the root of the SD-JWT duplicate-salt message ordering:
    // a default LHM must iterate in *insertion* order, NOT bucket order.
    //
    // Keys 8 then 2: insertion order is [8, 2], but a plain HashMap would
    // walk bucket 2 before bucket 8 and yield [2, 8]. The LHM must yield the
    // insertion order [8, 2] — proving it does not fall back to bucket order.
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, /*access_order=*/ false);

    let k8 = boxed_int(&mut ctx, 8);
    let v8 = boxed_int(&mut ctx, 80);
    let k2 = boxed_int(&mut ctx, 2);
    let v2 = boxed_int(&mut ctx, 20);

    call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), k8, v8],
    )
    .unwrap();
    call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), k2, v2],
    )
    .unwrap();

    let order = iter_keys_via_for_each(&reg, &mut ctx, lhm);
    assert_eq!(
        order,
        vec![k8, k2],
        "default LHM must iterate in insertion order [8,2], not bucket order [2,8]"
    );
}

#[test]
fn access_order_put_of_existing_key_moves_to_tail() {
    // Lock in the fix described in review §1.1 row 4 (C25): in an
    // access-order LHM, `put(k, v)` for a k already present must
    // reorder k to the tail, not just overwrite the value.
    //
    // Sequence: put k1, put k2, put k3, put k1 (existing) → expected
    // order k2, k3, k1.
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, /*access_order=*/ true);

    let k1 = boxed_int(&mut ctx, 1);
    let k2 = boxed_int(&mut ctx, 2);
    let k3 = boxed_int(&mut ctx, 3);
    let v1 = boxed_int(&mut ctx, 10);
    let v1b = boxed_int(&mut ctx, 11); // replacement value
    let v2 = boxed_int(&mut ctx, 20);
    let v3 = boxed_int(&mut ctx, 30);

    call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), k1, v1],
    )
    .unwrap();
    call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), k2, v2],
    )
    .unwrap();
    call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), k3, v3],
    )
    .unwrap();

    // Re-put k1 with a new value → must move k1 to tail.
    let prev = call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), k1, v1b],
    )
    .unwrap();
    assert_eq!(
        prev,
        Some(v1),
        "put-of-existing-key returns the previous value"
    );

    let order = iter_keys_via_for_each(&reg, &mut ctx, lhm);

    // The expected order — per C25 — is k2, k3, k1.
    //
    // NOTE: at the time this test was authored the underlying C25 fix
    // was not yet present in the source. The test pins the *intended*
    // contract; if `native_lhm_put` still updates the value in place
    // without calling `lhm_move_to_tail`, this assertion will fail and
    // surface the regression. That is the point — behavioural coverage
    // is exactly the mechanism that catches a missing access-order
    // reorder.
    assert_eq!(
        order,
        vec![k2, k3, k1],
        "access-order LHM put-of-existing-key must move k to tail; \
                expected [k2, k3, k1], got {order:?}"
    );
}

#[test]
fn plain_lhm_with_slot0_class_read_does_not_call_remove_eldest_entry() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, /*access_order=*/ false);

    let k1 = boxed_int(&mut ctx, 1);
    let v1 = boxed_int(&mut ctx, 10);
    call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), k1, v1],
    )
    .unwrap();

    ctx.set_object_class_id_for_test(lhm, ClassId::new(0));
    ctx.clear_invoke_virtual_log();

    let k2 = boxed_int(&mut ctx, 2);
    let v2 = boxed_int(&mut ctx, 20);
    call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), k2, v2],
    )
    .unwrap();

    assert!(
        ctx.invoke_virtual_log()
            .iter()
            .all(|(_, method, _, _)| method != "removeEldestEntry"),
        "slot-0/Object class-id reads from an LHM-native receiver must not dispatch Object.removeEldestEntry"
    );

    let order = iter_keys_via_for_each(&reg, &mut ctx, lhm);
    assert_eq!(order, vec![k1, k2]);
}

#[test]
fn lhm_subclass_still_calls_remove_eldest_entry() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let subclass_cid = ctx
        .ensure_class_initialized("test/LruLinkedHashMap")
        .unwrap();
    let lhm = ctx.alloc_object(subclass_cid, 8);

    call(
        &reg,
        &mut ctx,
        LHM,
        "<init>",
        "()V",
        &[Value::Object(Some(lhm))],
    )
    .unwrap();
    ctx.clear_invoke_virtual_log();

    let key = boxed_int(&mut ctx, 1);
    let value = boxed_int(&mut ctx, 10);
    call(
        &reg,
        &mut ctx,
        LHM,
        "put",
        PUT,
        &[Value::Object(Some(lhm)), key, value],
    )
    .unwrap();

    assert!(
        ctx.invoke_virtual_log()
            .iter()
            .any(|(_, method, desc, _)| method == "removeEldestEntry"
                && desc == "(Ljava/util/Map$Entry;)Z"),
        "real LinkedHashMap subclasses must keep their removeEldestEntry hook"
    );
}
