// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! Round 13 wave 8 (lane hashcompat3): the `--compatible` hash containers call
//! a key's `hashCode()` / `equals()` as often, on the same stored keys and in
//! the same order, as the JDK bodies they stand in for. Page
//! `r13w4-hashcompat-compatible-maps-diverge-from-jdk-comparison-order-
//! 20260928.md`, "Round 13 wave 8".
//!
//! * `CRATONVM_COMPAT_LHM_BUCKET_ORDER`: a `LinkedHashMap` bucket keeps its
//!   nodes in `put` order (tail append, order-keeping split), so a lookup
//!   compares the stored keys oldest first, as `HashMap.getNode` does.
//! * `CRATONVM_COMPAT_MAP_SINGLE_WALK` on `LinkedHashMap`: one `hashCode()` per
//!   conditional mutator; on `HashMap.putIfAbsent`: one `hashCode()`.
//! * `CRATONVM_COMPAT_CHM_SINGLE_HASH`: one `hashCode()` per
//!   `ConcurrentHashMap` `put` / `remove` / `putIfAbsent`.
//! * `CRATONVM_COMPAT_LHS_NODE_OPS`: `LinkedHashSet.removeFirst()` calls no
//!   `hashCode()`.
//! * `CRATONVM_COMPAT_SET_BULK_JDK`: `HashSet.retainAll(c)` asks `c.contains(e)`
//!   and unlinks without hashing; `removeAll(null)` throws.
//!
//! The keys are a user class, so each hash is a logged
//! `invoke_virtual("hashCode")`. Where the test needs colliding keys it
//! scripts the answers (`set_invoke_virtual_results`, consumed one per
//! `invoke_virtual` in call order); elsewhere the mock answers nothing and the
//! bucket hash is the identity hash.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{build_registry, call, new_concurrent_hashmap, new_linked_hashmap, MockCtx};
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};

const LHM: &str = "java/util/LinkedHashMap";
const CHM: &str = "java/util/concurrent/ConcurrentHashMap";
const HS: &str = "java/util/HashSet";
const LHS: &str = "java/util/LinkedHashSet";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const CONTAINS_KEY: &str = "(Ljava/lang/Object;)Z";
const REMOVE: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
const REMOVE_KV: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Z";
const REPLACE_KV: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const REPLACE_KOV: &str = "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z";
const ADD: &str = "(Ljava/lang/Object;)Z";
const BULK: &str = "(Ljava/util/Collection;)Z";

fn user_key(ctx: &mut MockCtx) -> ObjectRef {
    let cid = ctx.ensure_class_initialized("test/R13Hashcompat3Key").unwrap();
    ctx.alloc_object(cid, 1)
}

fn boxed_long(ctx: &mut MockCtx, v: i64) -> Value {
    let cid = ctx.ensure_class_initialized("java/lang/Long").unwrap();
    let obj = ctx.alloc_object(cid, 1);
    ctx.set_field(obj, 0, Value::Long(v));
    Value::Object(Some(obj))
}

fn calls_of(ctx: &MockCtx, method: &str) -> usize {
    ctx.invoke_virtual_log()
        .iter()
        .filter(|(_, m, _, _)| m == method)
        .count()
}

/// A scripted `hashCode()` answer: every key collides.
fn same_hash() -> MethodCallResult {
    Ok(Some(Value::Int(7)))
}

/// A scripted `equals()` answer: no two keys are equal.
fn not_equal() -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// The script for `n` `put`s of fresh colliding keys into one bucket: put `i`
/// hashes once, then compares against the `i` keys already there.
fn script_colliding_puts(n: usize) -> Vec<MethodCallResult> {
    let mut script = Vec::new();
    for i in 0..n {
        script.push(same_hash());
        for _ in 0..i {
            script.push(not_equal());
        }
    }
    script
}

/// Fill a fresh `LinkedHashMap` with `n` colliding keys, then look up one more
/// colliding key that equals none of them, and answer the stored keys its
/// `equals` calls were handed, in order.
fn lookup_order_after_colliding_puts(n: usize) -> (Vec<ObjectRef>, Vec<usize>) {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, false);
    let keys: Vec<ObjectRef> = (0..n).map(|_| user_key(&mut ctx)).collect();
    let value = boxed_long(&mut ctx, 1);
    ctx.set_invoke_virtual_results(script_colliding_puts(n));
    for k in &keys {
        call(
            &reg,
            &mut ctx,
            LHM,
            "put",
            PUT,
            &[Value::Object(Some(lhm)), Value::Object(Some(*k)), value],
        )
        .unwrap();
    }
    let probe = user_key(&mut ctx);
    ctx.clear_invoke_virtual_log();
    let mut script = vec![same_hash()];
    script.extend((0..n).map(|_| not_equal()));
    ctx.set_invoke_virtual_results(script);
    let found = call(
        &reg,
        &mut ctx,
        LHM,
        "containsKey",
        CONTAINS_KEY,
        &[Value::Object(Some(lhm)), Value::Object(Some(probe))],
    )
    .unwrap();
    assert_eq!(found, Some(Value::Int(0)), "the probe equals no stored key");
    let compared = ctx
        .invoke_virtual_log()
        .into_iter()
        .filter(|(_, m, _, _)| m == "equals")
        .map(|(_, _, _, args)| match args.first() {
            Some(Value::Object(Some(o))) => o.as_ptr() as usize,
            _ => 0,
        })
        .collect();
    (keys, compared)
}

fn addresses(keys: &[ObjectRef]) -> Vec<usize> {
    keys.iter().map(|k| k.as_ptr() as usize).collect()
}

#[test]
fn r13_hashcompat3_lhm_bucket_keeps_put_order() {
    // Three colliding keys: no resize. The head-pushing `put` compared the
    // newest first.
    let (keys, compared) = lookup_order_after_colliding_puts(3);
    assert_eq!(compared, addresses(&keys), "oldest stored key first");
}

#[test]
fn r13_hashcompat3_lhm_resize_split_keeps_bucket_order() {
    // Thirteen: the 13th put doubles the 16-bucket table first. The old
    // rebuild head-pushed the insertion-order list, reversing every chain.
    let (keys, compared) = lookup_order_after_colliding_puts(13);
    assert_eq!(compared, addresses(&keys), "the split keeps put order");
}

#[test]
fn r13_hashcompat3_lhm_conditional_mutators_hash_the_key_once() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, false);
    let key = Value::Object(Some(user_key(&mut ctx)));
    let v10 = boxed_long(&mut ctx, 10);
    call(&reg, &mut ctx, LHM, "put", PUT, &[Value::Object(Some(lhm)), key, v10]).unwrap();

    let before = calls_of(&ctx, "hashCode");
    let v20 = boxed_long(&mut ctx, 20);
    let old = call(
        &reg,
        &mut ctx,
        LHM,
        "replace",
        REPLACE_KV,
        &[Value::Object(Some(lhm)), key, v20],
    )
    .unwrap();
    assert_eq!(old, Some(v10), "replace answers the previous value");
    assert_eq!(calls_of(&ctx, "hashCode") - before, 1, "replace(k, v) hashes once");

    let before = calls_of(&ctx, "hashCode");
    let expect20 = boxed_long(&mut ctx, 20);
    let v30 = boxed_long(&mut ctx, 30);
    let swapped = call(
        &reg,
        &mut ctx,
        LHM,
        "replace",
        REPLACE_KOV,
        &[Value::Object(Some(lhm)), key, expect20, v30],
    )
    .unwrap();
    assert_eq!(swapped, Some(Value::Int(1)));
    assert_eq!(calls_of(&ctx, "hashCode") - before, 1, "replace(k, o, n) hashes once");

    let before = calls_of(&ctx, "hashCode");
    let wrong = boxed_long(&mut ctx, 99);
    let removed = call(
        &reg,
        &mut ctx,
        LHM,
        "remove",
        REMOVE_KV,
        &[Value::Object(Some(lhm)), key, wrong],
    )
    .unwrap();
    assert_eq!(removed, Some(Value::Int(0)), "a different value removes nothing");
    assert_eq!(calls_of(&ctx, "hashCode") - before, 1);

    let before = calls_of(&ctx, "hashCode");
    let expect30 = boxed_long(&mut ctx, 30);
    let removed = call(
        &reg,
        &mut ctx,
        LHM,
        "remove",
        REMOVE_KV,
        &[Value::Object(Some(lhm)), key, expect30],
    )
    .unwrap();
    assert_eq!(removed, Some(Value::Int(1)));
    assert_eq!(calls_of(&ctx, "hashCode") - before, 1, "remove(k, v) hashes once");
    let size = call(&reg, &mut ctx, LHM, "size", "()I", &[Value::Object(Some(lhm))]).unwrap();
    assert_eq!(size, Some(Value::Int(0)), "the matched entry is unlinked");
}

#[test]
fn r13_hashcompat3_chm_put_and_remove_hash_the_key_once() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let chm = new_concurrent_hashmap(&reg, &mut ctx);
    let key = Value::Object(Some(user_key(&mut ctx)));
    let v1 = boxed_long(&mut ctx, 1);
    let v2 = boxed_long(&mut ctx, 2);

    let before = calls_of(&ctx, "hashCode");
    call(&reg, &mut ctx, CHM, "put", PUT, &[Value::Object(Some(chm)), key, v1]).unwrap();
    assert_eq!(calls_of(&ctx, "hashCode") - before, 1, "put hashes once");

    let before = calls_of(&ctx, "hashCode");
    let old = call(&reg, &mut ctx, CHM, "put", PUT, &[Value::Object(Some(chm)), key, v2]).unwrap();
    assert_eq!(old, Some(v1), "an overwrite answers the previous value");
    assert_eq!(calls_of(&ctx, "hashCode") - before, 1, "an overwrite hashes once");

    let before = calls_of(&ctx, "hashCode");
    let old = call(&reg, &mut ctx, CHM, "remove", REMOVE, &[Value::Object(Some(chm)), key]).unwrap();
    assert_eq!(old, Some(v2), "remove answers the removed value");
    assert_eq!(calls_of(&ctx, "hashCode") - before, 1, "remove hashes once");

    let before = calls_of(&ctx, "hashCode");
    let absent = call(&reg, &mut ctx, CHM, "putIfAbsent", PUT, &[Value::Object(Some(chm)), key, v1])
        .unwrap();
    assert_eq!(absent, Some(Value::Object(None)), "putIfAbsent of an absent key inserts");
    assert_eq!(calls_of(&ctx, "hashCode") - before, 1, "putIfAbsent hashes once");

    let before = calls_of(&ctx, "hashCode");
    let kept = call(&reg, &mut ctx, CHM, "putIfAbsent", PUT, &[Value::Object(Some(chm)), key, v2])
        .unwrap();
    assert_eq!(kept, Some(v1), "putIfAbsent keeps the present value");
    assert_eq!(calls_of(&ctx, "hashCode") - before, 1);
}

#[test]
fn r13_hashcompat3_hash_map_put_if_absent_hashes_once() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = common::new_hashmap(&reg, &mut ctx);
    let key = Value::Object(Some(user_key(&mut ctx)));
    let v1 = boxed_long(&mut ctx, 1);
    let v2 = boxed_long(&mut ctx, 2);

    let before = calls_of(&ctx, "hashCode");
    let absent = call(
        &reg,
        &mut ctx,
        "java/util/HashMap",
        "putIfAbsent",
        PUT,
        &[Value::Object(Some(hm)), key, v1],
    )
    .unwrap();
    assert_eq!(absent, Some(Value::Object(None)), "an absent key is inserted");
    assert_eq!(calls_of(&ctx, "hashCode") - before, 1, "putVal hashes once");

    let before = calls_of(&ctx, "hashCode");
    let kept = call(
        &reg,
        &mut ctx,
        "java/util/HashMap",
        "putIfAbsent",
        PUT,
        &[Value::Object(Some(hm)), key, v2],
    )
    .unwrap();
    assert_eq!(kept, Some(v1), "a present non-null value is kept");
    assert_eq!(calls_of(&ctx, "hashCode") - before, 1);
    let got = call(
        &reg,
        &mut ctx,
        "java/util/HashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(hm)), key],
    )
    .unwrap();
    assert_eq!(got, Some(v1), "onlyIfAbsent did not overwrite");
}

fn new_set(reg: &NativeMethodRegistry, ctx: &mut MockCtx, class: &str) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class).unwrap();
    let set = ctx.alloc_object(cid, 4);
    call(reg, ctx, class, "<init>", "()V", &[Value::Object(Some(set))]).unwrap();
    set
}

fn set_size(reg: &NativeMethodRegistry, ctx: &mut MockCtx, class: &str, set: ObjectRef) -> Option<Value> {
    call(reg, ctx, class, "size", "()I", &[Value::Object(Some(set))]).unwrap()
}

/// A `LinkedHashSet`-shaped set the mock can serve: a `java/util/HashSet`
/// receiver whose backing (`HS_FIELD_MAP`, slot 0) is a `LinkedHashMap`.
///
/// Not `new_set(LHS)`: the mock has no class hierarchy (`is_subclass` is
/// `c == p`), so a `java/util/LinkedHashSet` receiver fails `hs_map_slot`'s
/// "is a `HashSet`" test, `hs_backing_map` answers `None`, and every `add`
/// answers `false` without storing anything. On the VM `LinkedHashSet extends
/// HashSet` and the receiver's own class serves it. The natives under test read
/// only the backing, so the receiver's class name does not matter to them.
fn new_linked_backed_set(reg: &NativeMethodRegistry, ctx: &mut MockCtx) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(HS).unwrap();
    let set = ctx.alloc_object(cid, 4);
    let backing = new_linked_hashmap(reg, ctx, false);
    ctx.set_field(set, 0, Value::Object(Some(backing)));
    set
}

#[test]
fn r13_hashcompat3_linked_hash_set_remove_first_does_not_hash() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let set = new_linked_backed_set(&reg, &mut ctx);
    let keys: Vec<ObjectRef> = (0..3).map(|_| user_key(&mut ctx)).collect();
    for k in &keys {
        let added = call(
            &reg,
            &mut ctx,
            HS,
            "add",
            ADD,
            &[Value::Object(Some(set)), Value::Object(Some(*k))],
        )
        .unwrap();
        assert_eq!(added, Some(Value::Int(1)));
    }
    let before = calls_of(&ctx, "hashCode");
    let first = call(
        &reg,
        &mut ctx,
        LHS,
        "removeFirst",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(set))],
    )
    .unwrap();
    assert_eq!(first, Some(Value::Object(Some(keys[0]))), "the eldest element");
    assert_eq!(calls_of(&ctx, "hashCode") - before, 0, "the node is removed, not the key");
    // Asked of the backing `LinkedHashMap`, not through `HashSet.size()`: in
    // the mock `lhm_set` cannot mirror the overlay's `size` into a real
    // `size` field (`resolve_field_index` knows no `LinkedHashMap` slots), so
    // `native_hs_size` -> `native_map_size` -> `map_state` reads the unmirrored
    // slot and answers 0 for any LinkedHashMap-backed set. The VM mirrors it.
    let backing = match ctx.get_field(set, 0) {
        Value::Object(Some(b)) => b,
        other => panic!("the set lost its backing map: {other:?}"),
    };
    let left = call(&reg, &mut ctx, LHM, "size", "()I", &[Value::Object(Some(backing))]).unwrap();
    assert_eq!(left, Some(Value::Int(2)));
}

#[test]
fn r13_hashcompat3_hash_set_retain_all_asks_the_argument() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let set = new_set(&reg, &mut ctx, HS);
    for _ in 0..3 {
        let k = user_key(&mut ctx);
        call(
            &reg,
            &mut ctx,
            HS,
            "add",
            ADD,
            &[Value::Object(Some(set)), Value::Object(Some(k))],
        )
        .unwrap();
    }
    let bag_cid = ctx.ensure_class_initialized("test/R13Hashcompat3Bag").unwrap();
    let bag = ctx.alloc_object(bag_cid, 1);
    ctx.clear_invoke_virtual_log();
    // `c.contains(e)` answers false for every element: retain nothing.
    ctx.set_invoke_virtual_results(vec![not_equal(), not_equal(), not_equal()]);
    let modified = call(
        &reg,
        &mut ctx,
        HS,
        "retainAll",
        BULK,
        &[Value::Object(Some(set)), Value::Object(Some(bag))],
    )
    .unwrap();
    assert_eq!(modified, Some(Value::Int(1)));
    let log = ctx.invoke_virtual_log();
    let asked: Vec<_> = log.iter().filter(|(_, m, _, _)| m == "contains").collect();
    assert_eq!(asked.len(), 3, "c.contains(e) once per element");
    assert!(
        asked.iter().all(|(recv, _, _, _)| *recv == bag.as_ptr() as usize),
        "the ARGUMENT decides membership"
    );
    assert_eq!(calls_of(&ctx, "hashCode"), 0, "Iterator.remove() does not hash");
    assert_eq!(calls_of(&ctx, "equals"), 0, "no equals of the set's own");
    assert_eq!(set_size(&reg, &mut ctx, HS, set), Some(Value::Int(0)));
}

#[test]
fn r13_hashcompat3_hash_set_remove_all_null_throws() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let set = new_set(&reg, &mut ctx, HS);
    let r = call(
        &reg,
        &mut ctx,
        HS,
        "removeAll",
        BULK,
        &[Value::Object(Some(set)), Value::Object(None)],
    );
    assert!(r.is_err(), "AbstractSet.removeAll(null) is Objects.requireNonNull: {r:?}");
}
