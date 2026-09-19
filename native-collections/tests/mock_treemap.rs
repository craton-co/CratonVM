// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! Behavioural coverage for `java.util.TreeMap`.
//!
//! Pins the sorted-iteration contract — keys come out in natural order.
//! This exercises the fast-mode `BTreeMap` backing as well as the array
//! fallback's binary-search insertion that the array-mode code path
//! exposes when `tm_force_array_mode` flips the receiver.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{boxed_char, boxed_int, build_registry, call, class_name_of, new_treemap, MockCtx};
use cratonvm_native_api::NativeContext;
use cratonvm_types::{ObjectRef, Value};

const TM: &str = "java/util/TreeMap";
const TS: &str = "java/util/TreeSet";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const GET: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";

#[test]
fn inserted_50_keys_iterate_in_sorted_order() {
    // Insert 50 keys in shuffled order and confirm the
    // firstKey / higherKey chain returns them in ascending order.
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let tm = new_treemap(&reg, &mut ctx);

    // Shuffled order (not 0..50 to make the sort test meaningful).
    let shuffled: Vec<i32> = (0..50).map(|i| ((i * 17) % 50) as i32).collect();
    // Sanity: every value in 0..50 appears exactly once.
    let mut sorted = shuffled.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        (0..50).collect::<Vec<_>>(),
        "test setup error: not a permutation of 0..50"
    );

    for &k in &shuffled {
        let kv = boxed_int(&mut ctx, k);
        let vv = boxed_int(&mut ctx, k * 100);
        call(
            &reg,
            &mut ctx,
            TM,
            "put",
            PUT,
            &[Value::Object(Some(tm)), kv, vv],
        )
        .unwrap();
    }

    let size = call(
        &reg,
        &mut ctx,
        TM,
        "size",
        "()I",
        &[Value::Object(Some(tm))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(50)));

    // Walk firstKey then higherKey(prev) until null.
    let mut walked: Vec<i32> = Vec::new();
    let first = call(
        &reg,
        &mut ctx,
        TM,
        "firstKey",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(tm))],
    )
    .unwrap();
    let mut cur = match first {
        Some(Value::Object(Some(o))) => Some(Value::Object(Some(o))),
        other => panic!("firstKey returned {:?}", other),
    };
    while let Some(k) = cur {
        // Extract the int value from the boxed Integer.
        let intval = match k {
            Value::Object(Some(o)) => match ctx_get_int(&ctx, o) {
                Some(i) => i,
                None => panic!("non-boxed key {:?}", k),
            },
            _ => panic!("non-object key {:?}", k),
        };
        walked.push(intval);
        if walked.len() > 200 {
            panic!("TreeMap iteration runaway");
        }
        let next = call(
            &reg,
            &mut ctx,
            TM,
            "higherKey",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(tm)), k],
        )
        .unwrap();
        cur = match next {
            Some(Value::Object(Some(o))) => Some(Value::Object(Some(o))),
            Some(Value::Object(None)) => None,
            _ => None,
        };
    }

    assert_eq!(
        walked,
        (0..50).collect::<Vec<_>>(),
        "TreeMap iteration must produce keys in ascending order"
    );
}

#[test]
fn round_trip_get_after_insert() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let tm = new_treemap(&reg, &mut ctx);

    let pairs: Vec<(i32, i32)> = vec![(5, 50), (1, 10), (9, 90), (3, 30)];
    let mut key_objs = Vec::new();
    for &(k, v) in &pairs {
        let kv = boxed_int(&mut ctx, k);
        let vv = boxed_int(&mut ctx, v);
        key_objs.push((kv, vv));
        call(
            &reg,
            &mut ctx,
            TM,
            "put",
            PUT,
            &[Value::Object(Some(tm)), kv, vv],
        )
        .unwrap();
    }
    for (k, v) in &key_objs {
        let got = call(
            &reg,
            &mut ctx,
            TM,
            "get",
            GET,
            &[Value::Object(Some(tm)), *k],
        )
        .unwrap();
        assert_eq!(got, Some(*v), "round-trip mismatch on key {:?}", k);
    }
}

#[test]
fn character_key_preserves_wrapper_type_on_readback() {
    // Regression for the `Locale.forLanguageTag` CCE (Integer→Character):
    // the fast-mode `TreeKey` used to collapse every int-field wrapper to
    // `I32`, so a `TreeMap<Character,?>` handed back `Integer` keys from
    // `firstKey()` / `keySet()` / `entrySet()`. `LocaleExtensions.toID`
    // `checkcast Character`s those keys → ClassCastException.
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let tm = new_treemap(&reg, &mut ctx);

    let k = boxed_char(&mut ctx, 'v');
    let v = boxed_int(&mut ctx, 42);
    call(
        &reg,
        &mut ctx,
        TM,
        "put",
        PUT,
        &[Value::Object(Some(tm)), k, v],
    )
    .unwrap();

    // firstKey() must come back as a Character, not an Integer.
    let first = call(
        &reg,
        &mut ctx,
        TM,
        "firstKey",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(tm))],
    )
    .unwrap();
    match first {
        Some(Value::Object(Some(o))) => {
            assert_eq!(
                class_name_of(&ctx, o).as_deref(),
                Some("java/lang/Character"),
                "firstKey() of a TreeMap<Character,?> must rebox as Character"
            );
        }
        other => panic!("firstKey returned {other:?}"),
    }

    // get with the SAME Character key must still find the value.
    let k_char = boxed_char(&mut ctx, 'v');
    let got_char = call(
        &reg,
        &mut ctx,
        TM,
        "get",
        GET,
        &[Value::Object(Some(tm)), k_char],
    )
    .unwrap();
    assert_eq!(
        got_char,
        Some(v),
        "get(Character('v')) must return the stored value"
    );

    // get with a numerically-equal Integer(118) must NOT match (HotSpot
    // parity: TreeMap.compare casts to Comparable and Character.compareTo
    // rejects an Integer — keys are NOT compared as bare numbers).
    let k_int = boxed_int(&mut ctx, 118);
    let got_int = call(
        &reg,
        &mut ctx,
        TM,
        "get",
        GET,
        &[Value::Object(Some(tm)), k_int],
    )
    .unwrap();
    assert_eq!(
        got_int,
        Some(Value::Object(None)),
        "get(Integer(118)) must NOT match a stored Character('v')"
    );
}

/// Read the inner `Int` value from a boxed `java.lang.Integer` heap
/// object — slot 0 carries the wrapped primitive.
fn ctx_get_int(ctx: &MockCtx, obj: cratonvm_types::ObjectRef) -> Option<i32> {
    match ctx.get_field(obj, 0) {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

#[test]
fn tree_map_liquibase_tie_comparator_preserves_insert_order() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let tm = new_treemap_with_tie_comparator(&reg, &mut ctx);
    let keys = tie_step_keys(&mut ctx);

    for (idx, key) in keys.iter().enumerate() {
        let value = boxed_int(&mut ctx, idx as i32);
        call(
            &reg,
            &mut ctx,
            TM,
            "put",
            PUT,
            &[Value::Object(Some(tm)), Value::Object(Some(*key)), value],
        )
        .unwrap();
    }

    let action = alloc_named_object(&mut ctx, "test/BiConsumer", 0);
    ctx.clear_invoke_virtual_log();
    call(
        &reg,
        &mut ctx,
        TM,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        &[Value::Object(Some(tm)), Value::Object(Some(action))],
    )
    .unwrap();

    let visited = ctx
        .invoke_virtual_log()
        .into_iter()
        .filter(|(_, method, desc, _)| {
            method == "accept" && desc == "(Ljava/lang/Object;Ljava/lang/Object;)V"
        })
        .map(|(_, _, _, args)| match args.first() {
            Some(Value::Object(Some(o))) => *o,
            other => panic!("unexpected TreeMap.forEach key arg: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        visited, keys,
        "TreeMap must compare new key against existing key so same-order comparator ties append like HotSpot"
    );
}

#[test]
fn comparator_backed_tree_map_clear_then_put_keeps_array_store_visible() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let tm = new_treemap_with_tie_comparator(&reg, &mut ctx);

    let stale_key = alloc_named_object(&mut ctx, "test/LiquibaseStep", 1);
    ctx.set_field(stale_key, 0, Value::Int(5));
    let stale_value = boxed_int(&mut ctx, 50);
    call(
        &reg,
        &mut ctx,
        TM,
        "put",
        PUT,
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(stale_key)),
            stale_value,
        ],
    )
    .unwrap();

    call(
        &reg,
        &mut ctx,
        TM,
        "clear",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    let key = alloc_named_object(&mut ctx, "test/LiquibaseStep", 1);
    ctx.set_field(key, 0, Value::Int(10));
    let value = boxed_int(&mut ctx, 100);
    call(
        &reg,
        &mut ctx,
        TM,
        "put",
        PUT,
        &[Value::Object(Some(tm)), Value::Object(Some(key)), value],
    )
    .unwrap();

    let size = call(
        &reg,
        &mut ctx,
        TM,
        "size",
        "()I",
        &[Value::Object(Some(tm))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(1)));

    let first = call(
        &reg,
        &mut ctx,
        TM,
        "firstKey",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(tm))],
    )
    .unwrap();
    assert_eq!(first, Some(Value::Object(Some(key))));

    let action = alloc_named_object(&mut ctx, "test/BiConsumer", 0);
    ctx.clear_invoke_virtual_log();
    call(
        &reg,
        &mut ctx,
        TM,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        &[Value::Object(Some(tm)), Value::Object(Some(action))],
    )
    .unwrap();
    let visited = ctx
        .invoke_virtual_log()
        .into_iter()
        .filter(|(_, method, desc, _)| {
            method == "accept" && desc == "(Ljava/lang/Object;Ljava/lang/Object;)V"
        })
        .map(|(_, _, _, args)| match args.first() {
            Some(Value::Object(Some(o))) => *o,
            other => panic!("unexpected TreeMap.forEach key arg: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(visited, vec![key]);
}

#[test]
fn tree_set_liquibase_tie_comparator_preserves_insert_order() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let ts = new_treeset_with_tie_comparator(&reg, &mut ctx);
    let keys = tie_step_keys(&mut ctx);

    for key in &keys {
        call(
            &reg,
            &mut ctx,
            TS,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(ts)), Value::Object(Some(*key))],
        )
        .unwrap();
    }

    let action = alloc_named_object(&mut ctx, "test/Consumer", 0);
    ctx.clear_invoke_virtual_log();
    call(
        &reg,
        &mut ctx,
        TS,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        &[Value::Object(Some(ts)), Value::Object(Some(action))],
    )
    .unwrap();

    let visited = ctx
        .invoke_virtual_log()
        .into_iter()
        .filter(|(_, method, desc, _)| method == "accept" && desc == "(Ljava/lang/Object;)V")
        .map(|(_, _, _, args)| match args.first() {
            Some(Value::Object(Some(o))) => *o,
            other => panic!("unexpected TreeSet.forEach element arg: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        visited, keys,
        "TreeSet must preserve same-order comparator tie insertion order like HotSpot"
    );
}

fn new_treemap_with_tie_comparator(
    reg: &cratonvm_native_api::NativeMethodRegistry,
    ctx: &mut MockCtx,
) -> ObjectRef {
    let tm = alloc_named_object(ctx, TM, 4);
    let cmp = alloc_named_object(ctx, "test/LiquibaseTieComparator", 0);
    call(
        reg,
        ctx,
        TM,
        "<init>",
        "(Ljava/util/Comparator;)V",
        &[Value::Object(Some(tm)), Value::Object(Some(cmp))],
    )
    .unwrap();
    tm
}

fn new_treeset_with_tie_comparator(
    reg: &cratonvm_native_api::NativeMethodRegistry,
    ctx: &mut MockCtx,
) -> ObjectRef {
    let ts = alloc_named_object(ctx, TS, 4);
    let cmp = alloc_named_object(ctx, "test/LiquibaseTieComparator", 0);
    call(
        reg,
        ctx,
        TS,
        "<init>",
        "(Ljava/util/Comparator;)V",
        &[Value::Object(Some(ts)), Value::Object(Some(cmp))],
    )
    .unwrap();
    ts
}

fn tie_step_keys(ctx: &mut MockCtx) -> Vec<ObjectRef> {
    [-1, -1, -1, -1, 1000]
        .into_iter()
        .map(|order| {
            let step = alloc_named_object(ctx, "test/LiquibaseStep", 1);
            ctx.set_field(step, 0, Value::Int(order));
            step
        })
        .collect()
}

fn alloc_named_object(ctx: &mut MockCtx, class_name: &str, fields: usize) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class_name).unwrap();
    ctx.alloc_object(cid, fields)
}
