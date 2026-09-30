// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! Round 13 wave 6 (lane hashtree): `HashMap.remove(k, v)`, `replace(k, v)`
//! and `replace(k, old, new)` hash the key ONCE, as the JDK's own bodies do
//! (`removeNode(.., matchValue, ..)` / `getNode`). The natives used to compose
//! `get` + (`containsKey`) + `put`/`remove`, so each call ran the key's
//! `hashCode()` two or three times. `CRATONVM_COMPAT_MAP_SINGLE_WALK`; page
//! `r13w4-hashcompat-compatible-maps-diverge-from-jdk-comparison-order-
//! 20260928.md`, item 5.
//!
//! The key is a user class (not a wrapper, not a `String`), so its hash is a
//! logged `invoke_virtual("hashCode")`; the mock answers nothing, so the
//! bucket hash is the identity hash and the SAME key object is used
//! throughout (it matches by `==`, with no `equals` call). Values are boxed
//! `Long`s, compared without Java. Every count below is 2 on the composite.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{build_registry, call, new_hashmap, MockCtx};
use cratonvm_types::{ObjectRef, Value};

const HM: &str = "java/util/HashMap";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const SIZE: &str = "()I";
const REMOVE_KV: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Z";
const REPLACE_KV: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const REPLACE_KOV: &str = "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z";

fn boxed_long(ctx: &mut MockCtx, v: i64) -> Value {
    let cid = ctx.ensure_class_initialized("java/lang/Long").unwrap();
    let obj = ctx.alloc_object(cid, 1);
    ctx.set_field(obj, 0, Value::Long(v));
    Value::Object(Some(obj))
}

fn user_key(ctx: &mut MockCtx) -> Value {
    let cid = ctx.ensure_class_initialized("test/R13HashtreeKey").unwrap();
    Value::Object(Some(ctx.alloc_object(cid, 1)))
}

fn hash_calls(ctx: &MockCtx) -> usize {
    ctx.invoke_virtual_log()
        .iter()
        .filter(|(_, method, _, _)| method == "hashCode")
        .count()
}

fn size(reg: &cratonvm_native_api::NativeMethodRegistry, ctx: &mut MockCtx, m: ObjectRef) -> i32 {
    match call(reg, ctx, HM, "size", SIZE, &[Value::Object(Some(m))]).unwrap() {
        Some(Value::Int(n)) => n,
        other => panic!("size returned {other:?}"),
    }
}

#[test]
fn r13_hashtree_conditional_mutators_hash_the_key_once() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);
    let key = user_key(&mut ctx);
    let v10 = boxed_long(&mut ctx, 10);
    call(&reg, &mut ctx, HM, "put", PUT, &[Value::Object(Some(hm)), key, v10]).unwrap();

    // replace(k, v): the old value back, one hashCode().
    let before = hash_calls(&ctx);
    let v20 = boxed_long(&mut ctx, 20);
    let old = call(
        &reg,
        &mut ctx,
        HM,
        "replace",
        REPLACE_KV,
        &[Value::Object(Some(hm)), key, v20],
    )
    .unwrap();
    assert_eq!(old, Some(v10), "replace answers the previous value");
    assert_eq!(hash_calls(&ctx) - before, 1, "replace(k, v) hashes once");

    // replace(k, old, new), matching (an equal but distinct Long).
    let before = hash_calls(&ctx);
    let expect20 = boxed_long(&mut ctx, 20);
    let v30 = boxed_long(&mut ctx, 30);
    let swapped = call(
        &reg,
        &mut ctx,
        HM,
        "replace",
        REPLACE_KOV,
        &[Value::Object(Some(hm)), key, expect20, v30],
    )
    .unwrap();
    assert_eq!(swapped, Some(Value::Int(1)));
    assert_eq!(hash_calls(&ctx) - before, 1, "replace(k, o, n) hashes once");

    // remove(k, v) with a different value: false, nothing removed.
    let before = hash_calls(&ctx);
    let wrong = boxed_long(&mut ctx, 99);
    let removed = call(
        &reg,
        &mut ctx,
        HM,
        "remove",
        REMOVE_KV,
        &[Value::Object(Some(hm)), key, wrong],
    )
    .unwrap();
    assert_eq!(removed, Some(Value::Int(0)));
    assert_eq!(hash_calls(&ctx) - before, 1);
    assert_eq!(size(&reg, &mut ctx, hm), 1);

    // remove(k, v) with the current value: true, and the entry is gone.
    let before = hash_calls(&ctx);
    let expect30 = boxed_long(&mut ctx, 30);
    let removed = call(
        &reg,
        &mut ctx,
        HM,
        "remove",
        REMOVE_KV,
        &[Value::Object(Some(hm)), key, expect30],
    )
    .unwrap();
    assert_eq!(removed, Some(Value::Int(1)));
    assert_eq!(hash_calls(&ctx) - before, 1, "remove(k, v) hashes once");
    assert_eq!(size(&reg, &mut ctx, hm), 0, "the matched entry is unlinked");

    // An absent key: replace answers null and inserts nothing.
    let other = user_key(&mut ctx);
    let v1 = boxed_long(&mut ctx, 1);
    let out = call(
        &reg,
        &mut ctx,
        HM,
        "replace",
        REPLACE_KV,
        &[Value::Object(Some(hm)), other, v1],
    )
    .unwrap();
    assert_eq!(out, Some(Value::Object(None)));
    assert_eq!(size(&reg, &mut ctx, hm), 0);
}
