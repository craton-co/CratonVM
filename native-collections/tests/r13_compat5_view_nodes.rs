// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! Round 13 wave 10 (lane compat5): the `--compatible` `HashMap` views and the
//! `ConcurrentHashMap` remapping methods call `hashCode()` as the JDK bodies
//! do, and a `values()` iterator removes the node it stands on. Pages
//! `r13w9-hashcompat4-map-views-call-hashcode-FIXED-20260928.md`,
//! `r13w9-hashcompat4-overlay-remap-order-FIXED-20260928.md` and
//! `r13w8-hashcompat3-compatible-collection-residuals-20260928.md`,
//! "Round 13 wave 10".
//!
//! * `CRATONVM_COMPAT_VIEW_BUILD_BY_NODE`: building and resyncing a
//!   `keySet()` calls no `hashCode()`.
//! * `CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE`: `keySet().remove(k)` and
//!   `entrySet().remove(e)` hash once, a view iterator's `remove()` never, and
//!   a `values()` iterator removes ITS node, not the first equal value.
//! * `CRATONVM_COMPAT_OVERLAY_REMAP_ORDER`: an `Integer`-keyed
//!   `computeIfAbsent` insert heads its bucket.
//! * `CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH`: `ConcurrentHashMap`
//!   `computeIfAbsent` / `merge` hash once.
//!
//! The keys are a user class, so each hash is a logged
//! `invoke_virtual("hashCode")`; the mock answers `invoke_virtual` from a
//! script consumed one per call, and with `Ok(None)` once it is empty (a
//! `hashCode()` answering nothing falls back to the identity hash). Every
//! test that builds a `MockCtx` gives it its own VM identity: two contexts on
//! one thread otherwise share the per-thread `(vm, ClassId)` caches.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{
    boxed_int, build_registry, call, class_name_of, new_concurrent_hashmap, new_hashmap, MockCtx,
};
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::{ObjectRef, Value};

const HM: &str = "java/util/HashMap";
const HS: &str = "java/util/HashSet";
const CHM: &str = "java/util/concurrent/ConcurrentHashMap";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const GET: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
const SET_REMOVE: &str = "(Ljava/lang/Object;)Z";
const COMPUTE_IF_ABSENT: &str =
    "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;";
const MERGE: &str =
    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;";
const FOR_EACH: &str = "(Ljava/util/function/BiConsumer;)V";

fn user_object(ctx: &mut MockCtx, class: &str) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class).unwrap();
    ctx.alloc_object(cid, 1)
}

fn user_key(ctx: &mut MockCtx) -> ObjectRef {
    user_object(ctx, "test/R13Compat5Key")
}

fn value_obj(ctx: &mut MockCtx) -> Value {
    Value::Object(Some(user_object(ctx, "test/R13Compat5Value")))
}

fn obj(o: ObjectRef) -> Value {
    Value::Object(Some(o))
}

fn calls_of(ctx: &MockCtx, method: &str) -> usize {
    ctx.invoke_virtual_log()
        .iter()
        .filter(|(_, m, _, _)| m == method)
        .count()
}

fn fresh_ctx(vm: usize) -> MockCtx {
    let mut ctx = MockCtx::new();
    ctx.set_vm_identity(vm);
    ctx
}

fn put(reg: &NativeMethodRegistry, ctx: &mut MockCtx, map: ObjectRef, k: Value, v: Value) {
    call(reg, ctx, HM, "put", PUT, &[obj(map), k, v]).unwrap();
}

fn map_size(reg: &NativeMethodRegistry, ctx: &mut MockCtx, map: ObjectRef) -> Option<Value> {
    call(reg, ctx, HM, "size", "()I", &[obj(map)]).unwrap()
}

fn view(reg: &NativeMethodRegistry, ctx: &mut MockCtx, map: ObjectRef, which: &str) -> ObjectRef {
    match call(reg, ctx, HM, which, "()Ljava/util/Set;", &[obj(map)]).unwrap() {
        Some(Value::Object(Some(v))) => v,
        other => panic!("{which}() answered {other:?}"),
    }
}

/// A `HashMap` holding `n` user keys whose `hashCode()` answers `hash`
/// (one bucket when equal), each mapped to a fresh value.
fn map_of_user_keys(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    hashes: &[i32],
) -> (ObjectRef, Vec<ObjectRef>) {
    let hm = new_hashmap(reg, ctx);
    let mut keys = Vec::new();
    for h in hashes {
        let k = user_key(ctx);
        let v = value_obj(ctx);
        // hashCode(k), then `equals` against each same-hash key already in:
        // the mock answers `false` (Int 0) for those.
        let mut script = vec![Ok(Some(Value::Int(*h)))];
        script.extend((0..keys.len()).map(|_| Ok(Some(Value::Int(0)))));
        ctx.set_invoke_virtual_results(script);
        put(reg, ctx, hm, obj(k), v);
        keys.push(k);
    }
    ctx.set_invoke_virtual_results(Vec::new());
    (hm, keys)
}

/// `(key, value)` pairs `forEach` hands its action, in order.
fn for_each_pairs(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    map: ObjectRef,
) -> Vec<(Value, Value)> {
    let action = user_object(ctx, "test/R13Compat5Action");
    ctx.set_invoke_virtual_results(Vec::new());
    ctx.clear_invoke_virtual_log();
    call(reg, ctx, HM, "forEach", FOR_EACH, &[obj(map), obj(action)]).unwrap();
    ctx.invoke_virtual_log()
        .into_iter()
        .filter(|(_, m, _, _)| m == "accept")
        .map(|(_, _, _, args)| (args[0], args[1]))
        .collect()
}

#[test]
fn r13_compat5_key_set_build_and_resync_call_no_hash_code() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x4335_0001);
    let (hm, _) = map_of_user_keys(&reg, &mut ctx, &[7, 7, 9]);
    ctx.clear_invoke_virtual_log();
    let ks = view(&reg, &mut ctx, hm, "keySet");
    let n = call(&reg, &mut ctx, HS, "size", "()I", &[obj(ks)]).unwrap();
    assert_eq!(n, Some(Value::Int(3)));
    assert_eq!(calls_of(&ctx, "hashCode"), 0, "HotSpot's keySet() hashes nothing");
    assert_eq!(calls_of(&ctx, "equals"), 0, "nor compares keys");

    // A structural change of the source makes the next read resync.
    let k = user_key(&mut ctx);
    let v = value_obj(&mut ctx);
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(11)))]);
    put(&reg, &mut ctx, hm, obj(k), v);
    ctx.set_invoke_virtual_results(Vec::new());
    ctx.clear_invoke_virtual_log();
    let n = call(&reg, &mut ctx, HS, "size", "()I", &[obj(ks)]).unwrap();
    assert_eq!(n, Some(Value::Int(4)), "the view is live");
    assert_eq!(calls_of(&ctx, "hashCode"), 0, "the resync hashes nothing either");
}

#[test]
fn r13_compat5_key_set_remove_hashes_once() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x4335_0002);
    let (hm, keys) = map_of_user_keys(&reg, &mut ctx, &[5, 6]);
    let ks = view(&reg, &mut ctx, hm, "keySet");
    ctx.clear_invoke_virtual_log();
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(5)))]);
    let removed = call(&reg, &mut ctx, HS, "remove", SET_REMOVE, &[obj(ks), obj(keys[0])]).unwrap();
    assert_eq!(removed, Some(Value::Int(1)));
    assert_eq!(calls_of(&ctx, "hashCode"), 1, "removeNode(hash(key), ..): one hash");
    assert_eq!(map_size(&reg, &mut ctx, hm), Some(Value::Int(1)));

    // An absent key: one hash, nothing removed, `false`.
    ctx.clear_invoke_virtual_log();
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(5)))]);
    let removed = call(&reg, &mut ctx, HS, "remove", SET_REMOVE, &[obj(ks), obj(keys[0])]).unwrap();
    assert_eq!(removed, Some(Value::Int(0)));
    assert_eq!(calls_of(&ctx, "hashCode"), 1);
    assert_eq!(map_size(&reg, &mut ctx, hm), Some(Value::Int(1)));
}

/// A live entry as the natives mint one: `AbstractMap$SimpleEntry` with the
/// source map in slot 2.
fn live_entry(ctx: &mut MockCtx, key: ObjectRef, value: Value, source: ObjectRef) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized("java/util/AbstractMap$SimpleEntry")
        .unwrap();
    let e = ctx.alloc_object(cid, 3);
    ctx.set_field(e, 0, obj(key));
    ctx.set_field(e, 1, value);
    ctx.set_field(e, 2, obj(source));
    e
}

#[test]
fn r13_compat5_entry_set_remove_hashes_once() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x4335_0003);
    let hm = new_hashmap(&reg, &mut ctx);
    let k = user_key(&mut ctx);
    let v = value_obj(&mut ctx);
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(3)))]);
    put(&reg, &mut ctx, hm, obj(k), v);
    let es = view(&reg, &mut ctx, hm, "entrySet");
    let e = live_entry(&mut ctx, k, v, hm);
    ctx.clear_invoke_virtual_log();
    // hashCode(k) only: the stored key and value are the entry's own
    // objects, so `==` decides both comparisons.
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(3)))]);
    let removed = call(&reg, &mut ctx, HS, "remove", SET_REMOVE, &[obj(es), obj(e)]).unwrap();
    assert_eq!(removed, Some(Value::Int(1)));
    assert_eq!(
        calls_of(&ctx, "hashCode"),
        1,
        "removeNode(hash(key), key, value, true, ..): one hash, where containsKey + get + remove made three"
    );
    assert_eq!(map_size(&reg, &mut ctx, hm), Some(Value::Int(0)));
}

#[test]
fn r13_compat5_key_set_iterator_remove_hashes_nothing() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x4335_0004);
    let (hm, _) = map_of_user_keys(&reg, &mut ctx, &[1, 2, 3]);
    let ks = view(&reg, &mut ctx, hm, "keySet");
    let itr = match call(&reg, &mut ctx, HS, "iterator", "()Ljava/util/Iterator;", &[obj(ks)])
        .unwrap()
    {
        Some(Value::Object(Some(itr))) => itr,
        other => panic!("iterator() answered {other:?}"),
    };
    let carrier = class_name_of(&ctx, itr).expect("a named iterator carrier");
    call(&reg, &mut ctx, &carrier, "next", "()Ljava/lang/Object;", &[obj(itr)]).unwrap();
    ctx.set_invoke_virtual_results(Vec::new());
    ctx.clear_invoke_virtual_log();
    call(&reg, &mut ctx, &carrier, "remove", "()V", &[obj(itr)]).unwrap();
    assert_eq!(
        calls_of(&ctx, "hashCode"),
        0,
        "KeyIterator.remove() is removeNode(p.hash, ..): the stored hash"
    );
    assert_eq!(map_size(&reg, &mut ctx, hm), Some(Value::Int(2)));
}

#[test]
fn r13_compat5_values_iterator_removes_its_own_node() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x4335_0005);
    let hm = new_hashmap(&reg, &mut ctx);
    let a = user_key(&mut ctx);
    let b = user_key(&mut ctx);
    let shared = value_obj(&mut ctx);
    // a in bucket 1, b in bucket 2: iteration order {a, b}; one value object.
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(1)))]);
    put(&reg, &mut ctx, hm, obj(a), shared);
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(2)))]);
    put(&reg, &mut ctx, hm, obj(b), shared);
    ctx.set_invoke_virtual_results(Vec::new());
    // `HashMap$ValueIterator` must report a REAL declared width (the mock
    // answers 0): `HashIterator`'s `next`, `current`, `expectedModCount`,
    // `index` and the enclosing `this$0`, five. The natives recognise their
    // own mint by `class_num_total_fields + 3` behind an early-out for
    // anything four fields or narrower; minted three wide, the iterator takes
    // that early-out and every `next`/`remove` is delegated to bytecode the
    // mock does not have (`Ok(None)`, nothing removed) -- see the same note
    // in `gc_native_pins.rs`.
    ctx.declare_class_fields("java/util/HashMap$ValueIterator", 5);
    let values = match call(&reg, &mut ctx, HM, "values", "()Ljava/util/Collection;", &[obj(hm)])
        .unwrap()
    {
        Some(Value::Object(Some(v))) => v,
        other => panic!("values() answered {other:?}"),
    };
    let values_class = class_name_of(&ctx, values).expect("a named values carrier");
    let itr = match call(
        &reg,
        &mut ctx,
        &values_class,
        "iterator",
        "()Ljava/util/Iterator;",
        &[obj(values)],
    )
    .unwrap()
    {
        Some(Value::Object(Some(itr))) => itr,
        other => panic!("values().iterator() answered {other:?}"),
    };
    let carrier = class_name_of(&ctx, itr).expect("a named values iterator");
    for _ in 0..2 {
        // A delegated (not native) `next()` answers nothing: fail loudly here
        // rather than as a silent no-op `remove()` below.
        let got =
            call(&reg, &mut ctx, &carrier, "next", "()Ljava/lang/Object;", &[obj(itr)]).unwrap();
        assert_eq!(got, Some(shared), "the native values iterator hands out the value");
    }
    ctx.clear_invoke_virtual_log();
    call(&reg, &mut ctx, &carrier, "remove", "()V", &[obj(itr)]).unwrap();
    assert_eq!(calls_of(&ctx, "hashCode"), 0, "ValueIterator.remove() hashes nothing");
    let left = for_each_pairs(&reg, &mut ctx, hm);
    assert_eq!(
        left,
        vec![(obj(a), shared)],
        "the second value's node went -- HotSpot keeps {{a}}, the by-value removal dropped a"
    );
}

#[test]
fn r13_compat5_integer_compute_if_absent_heads_its_bucket() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x4335_0006);
    let hm = new_hashmap(&reg, &mut ctx);
    let one = boxed_int(&mut ctx, 1);
    let seventeen = boxed_int(&mut ctx, 17);
    let v1 = value_obj(&mut ctx);
    let v2 = value_obj(&mut ctx);
    put(&reg, &mut ctx, hm, one, v1);
    let f = user_object(&mut ctx, "test/R13Compat5Function");
    ctx.set_invoke_virtual_results(vec![Ok(Some(v2))]);
    let got = call(
        &reg,
        &mut ctx,
        HM,
        "computeIfAbsent",
        COMPUTE_IF_ABSENT,
        &[obj(hm), seventeen, obj(f)],
    )
    .unwrap();
    assert_eq!(got, Some(v2));
    let order: Vec<Value> = for_each_pairs(&reg, &mut ctx, hm)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        order,
        vec![seventeen, one],
        "HotSpot: m.put(1, ..); m.computeIfAbsent(17, ..) iterates {{17, 1}}"
    );
}

#[test]
fn r13_compat5_chm_compute_if_absent_and_merge_hash_once() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x4335_0007);
    let chm = new_concurrent_hashmap(&reg, &mut ctx);
    let k = user_key(&mut ctx);
    let v = value_obj(&mut ctx);
    let f = user_object(&mut ctx, "test/R13Compat5Function");
    ctx.clear_invoke_virtual_log();
    // hashCode(k), then f.apply(k) -> v.
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(4))), Ok(Some(v))]);
    let got = call(
        &reg,
        &mut ctx,
        CHM,
        "computeIfAbsent",
        COMPUTE_IF_ABSENT,
        &[obj(chm), obj(k), obj(f)],
    )
    .unwrap();
    assert_eq!(got, Some(v));
    assert_eq!(
        calls_of(&ctx, "hashCode"),
        1,
        "spread(key.hashCode()) once, where the reservation protocol made five"
    );

    // merge of a present key: hashCode(k), f.apply(v, w) -> w.
    let w = value_obj(&mut ctx);
    ctx.clear_invoke_virtual_log();
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(4))), Ok(Some(w))]);
    let got = call(&reg, &mut ctx, CHM, "merge", MERGE, &[obj(chm), obj(k), w, obj(f)]).unwrap();
    assert_eq!(got, Some(w));
    assert_eq!(calls_of(&ctx, "hashCode"), 1, "merge hashes once");
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(4)))]);
    let now = call(&reg, &mut ctx, CHM, "get", GET, &[obj(chm), obj(k)]).unwrap();
    assert_eq!(now, Some(w));
}
