// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! Round 13 wave 9 (lane hashcompat4): the `--compatible` `HashMap` /
//! `LinkedHashMap` / `HashSet` natives take the JDK bodies' own shape for the
//! remapping methods, `Iterator.remove()`, a live entry's `setValue`, the
//! reversed `LinkedHashMap` and `Map.equals`. Pages
//! `r13w8-hashcompat3-compatible-collection-residuals-20260928.md` and
//! `r13w4-hashcompat-compatible-maps-diverge-from-jdk-comparison-order-
//! 20260928.md`, "Round 13 wave 9".
//!
//! * `CRATONVM_COMPAT_MAP_COMPUTE_JDK`: `computeIfAbsent` / `compute` /
//!   `computeIfPresent` / `merge` hash once, link a new key at its bucket's
//!   HEAD, treat a present null value as present, and (access-ordered
//!   `LinkedHashMap`) move the entry only after a non-null result.
//! * `CRATONVM_COMPAT_ITR_REMOVE_BY_NODE`: `Iterator.remove()` on an ordinary
//!   set unlinks the node, with no `hashCode()`.
//! * `CRATONVM_COMPAT_ENTRY_SET_VALUE_IN_PLACE`: a live entry's `setValue`
//!   writes the node: no reorder, no resurrection of a removed key.
//! * `CRATONVM_COMPAT_LHM_REVERSED_BY_NODE`: `LinkedHashMap.reversed()` calls
//!   no `hashCode()`.
//! * `CRATONVM_COMPAT_MAP_EQUALS_JDK`: `Map.equals` asks `value.equals(..)`
//!   with no `==` short cut.
//!
//! The keys are a user class, so each hash is a logged
//! `invoke_virtual("hashCode")`. The mock answers `invoke_virtual` from a
//! script consumed one per call in call order, and with `Ok(None)` once the
//! script is empty (a `hashCode()` answering nothing falls back to the
//! identity hash; a function answering nothing returns null).

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{build_registry, call, class_name_of, new_hashmap, new_linked_hashmap, MockCtx};
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

const HM: &str = "java/util/HashMap";
const LHM: &str = "java/util/LinkedHashMap";
const HS: &str = "java/util/HashSet";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const REMOVE: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
const COMPUTE_IF_ABSENT: &str =
    "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;";
const REMAP: &str = "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;";
const FOR_EACH: &str = "(Ljava/util/function/BiConsumer;)V";
const ADD: &str = "(Ljava/lang/Object;)Z";

fn user_object(ctx: &mut MockCtx, class: &str) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class).unwrap();
    ctx.alloc_object(cid, 1)
}

fn user_key(ctx: &mut MockCtx) -> ObjectRef {
    user_object(ctx, "test/R13Hashcompat4Key")
}

fn value_obj(ctx: &mut MockCtx) -> Value {
    Value::Object(Some(user_object(ctx, "test/R13Hashcompat4Value")))
}

fn calls_of(ctx: &MockCtx, method: &str) -> usize {
    ctx.invoke_virtual_log()
        .iter()
        .filter(|(_, m, _, _)| m == method)
        .count()
}

fn obj(o: ObjectRef) -> Value {
    Value::Object(Some(o))
}

fn put(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    class: &str,
    map: ObjectRef,
    k: ObjectRef,
    v: Value,
) {
    call(reg, ctx, class, "put", PUT, &[obj(map), obj(k), v]).unwrap();
}

fn size(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    class: &str,
    target: ObjectRef,
) -> Option<Value> {
    call(reg, ctx, class, "size", "()I", &[obj(target)]).unwrap()
}

/// Run `forEach` and answer the `(key, value)` pairs the action was handed, in
/// order. The action answers nothing.
fn for_each_pairs(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    class: &str,
    map: ObjectRef,
) -> Vec<(Value, Value)> {
    let action = user_object(ctx, "test/R13Hashcompat4Action");
    ctx.set_invoke_virtual_results(Vec::new());
    ctx.clear_invoke_virtual_log();
    call(
        reg,
        ctx,
        class,
        "forEach",
        FOR_EACH,
        &[Value::Object(Some(map)), Value::Object(Some(action))],
    )
    .unwrap();
    ctx.invoke_virtual_log()
        .into_iter()
        .filter(|(_, m, _, _)| m == "accept")
        .map(|(_, _, _, args)| (args[0], args[1]))
        .collect()
}

fn keys_of(pairs: &[(Value, Value)]) -> Vec<Value> {
    pairs.iter().map(|(k, _)| *k).collect()
}

fn function(ctx: &mut MockCtx) -> Value {
    Value::Object(Some(user_object(ctx, "test/R13Hashcompat4Function")))
}

fn thrown() -> MethodCallResult {
    Err(RuntimeError::IllegalStateException {
        message: "from the function".to_string(),
    }
    .into())
}

#[test]
fn r13_hashcompat4_compute_if_absent_hashes_once_and_links_at_the_bucket_head() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);
    let k1 = user_key(&mut ctx);
    let k2 = user_key(&mut ctx);
    let v1 = value_obj(&mut ctx);
    let v2 = value_obj(&mut ctx);
    // Both keys hash to 7: one bucket.
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(7)))]);
    put(&reg, &mut ctx, HM, hm, k1, v1);

    let f = function(&mut ctx);
    ctx.clear_invoke_virtual_log();
    // hashCode(k2), k2.equals(k1) -> false, f.apply(k2) -> v2.
    ctx.set_invoke_virtual_results(vec![
        Ok(Some(Value::Int(7))),
        Ok(Some(Value::Int(0))),
        Ok(Some(v2)),
    ]);
    let got = call(
        &reg,
        &mut ctx,
        HM,
        "computeIfAbsent",
        COMPUTE_IF_ABSENT,
        &[Value::Object(Some(hm)), Value::Object(Some(k2)), f],
    )
    .unwrap();
    assert_eq!(got, Some(v2), "computeIfAbsent answers the function's value");
    assert_eq!(calls_of(&ctx, "hashCode"), 1, "one hash(key), as the JDK body");
    assert_eq!(calls_of(&ctx, "equals"), 1, "one walk of the chain");
    assert_eq!(size(&reg, &mut ctx, HM, hm), Some(Value::Int(2)));

    // `tab[i] = newNode(hash, key, v, first)`: the new key is the bucket head,
    // so HotSpot iterates {k2, k1}.
    let order = keys_of(&for_each_pairs(&reg, &mut ctx, HM, hm));
    assert_eq!(
        order,
        vec![Value::Object(Some(k2)), Value::Object(Some(k1))],
        "a computeIfAbsent insert heads its bucket"
    );
}

#[test]
fn r13_hashcompat4_compute_if_present_hashes_once_unless_it_removes() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);
    let k = user_key(&mut ctx);
    let v1 = value_obj(&mut ctx);
    let v2 = value_obj(&mut ctx);
    put(&reg, &mut ctx, HM, hm, k, v1);
    let f = function(&mut ctx);

    // hashCode(k) (identity), f.apply(k, v1) -> v2: the node is written.
    ctx.clear_invoke_virtual_log();
    ctx.set_invoke_virtual_results(vec![Ok(None), Ok(Some(v2))]);
    let got = call(
        &reg,
        &mut ctx,
        HM,
        "computeIfPresent",
        REMAP,
        &[Value::Object(Some(hm)), Value::Object(Some(k)), f],
    )
    .unwrap();
    assert_eq!(got, Some(v2));
    assert_eq!(calls_of(&ctx, "hashCode"), 1, "getNode hashes once");

    // A null result removes: `removeNode(hash(key), ..)` hashes a second time.
    ctx.clear_invoke_virtual_log();
    ctx.set_invoke_virtual_results(Vec::new());
    let got = call(
        &reg,
        &mut ctx,
        HM,
        "computeIfPresent",
        REMAP,
        &[Value::Object(Some(hm)), Value::Object(Some(k)), f],
    )
    .unwrap();
    assert_eq!(got, Some(Value::Object(None)));
    assert_eq!(calls_of(&ctx, "hashCode"), 2, "the removal re-hashes, as the JDK does");
    assert_eq!(size(&reg, &mut ctx, HM, hm), Some(Value::Int(0)));
}

#[test]
fn r13_hashcompat4_compute_null_result_removes_a_present_null_mapping() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);
    let k = user_key(&mut ctx);
    put(&reg, &mut ctx, HM, hm, k, Value::Object(None));
    assert_eq!(size(&reg, &mut ctx, HM, hm), Some(Value::Int(1)));
    let f = function(&mut ctx);
    ctx.set_invoke_virtual_results(Vec::new());
    // `old != null` (the node exists) and `v == null`: removeNode.
    let got = call(
        &reg,
        &mut ctx,
        HM,
        "compute",
        REMAP,
        &[Value::Object(Some(hm)), Value::Object(Some(k)), f],
    )
    .unwrap();
    assert_eq!(got, Some(Value::Object(None)));
    assert_eq!(
        size(&reg, &mut ctx, HM, hm),
        Some(Value::Int(0)),
        "a key mapped to null is present, and compute -> null removes it"
    );
}

#[test]
fn r13_hashcompat4_access_ordered_compute_moves_only_after_a_result() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, true);
    let a = user_key(&mut ctx);
    let b = user_key(&mut ctx);
    let va = value_obj(&mut ctx);
    let vb = value_obj(&mut ctx);
    put(&reg, &mut ctx, LHM, lhm, a, va);
    put(&reg, &mut ctx, LHM, lhm, b, vb);
    let f = function(&mut ctx);
    // hashCode(a), then the function throws.
    ctx.set_invoke_virtual_results(vec![Ok(None), thrown()]);
    let r = call(
        &reg,
        &mut ctx,
        HM,
        "computeIfPresent",
        REMAP,
        &[Value::Object(Some(lhm)), Value::Object(Some(a)), f],
    );
    assert!(r.is_err(), "the function's exception propagates");
    let order = keys_of(&for_each_pairs(&reg, &mut ctx, LHM, lhm));
    assert_eq!(
        order,
        vec![Value::Object(Some(a)), Value::Object(Some(b))],
        "afterNodeAccess runs only after a non-null result"
    );
}

#[test]
fn r13_hashcompat4_linked_compute_if_absent_hashes_once() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, false);
    let k = user_key(&mut ctx);
    let v = value_obj(&mut ctx);
    let f = function(&mut ctx);
    ctx.clear_invoke_virtual_log();
    ctx.set_invoke_virtual_results(vec![Ok(None), Ok(Some(v))]);
    let got = call(
        &reg,
        &mut ctx,
        LHM,
        "computeIfAbsent",
        COMPUTE_IF_ABSENT,
        &[Value::Object(Some(lhm)), Value::Object(Some(k)), f],
    )
    .unwrap();
    assert_eq!(got, Some(v));
    assert_eq!(calls_of(&ctx, "hashCode"), 1, "one hash(key) for lookup and insert");
    assert_eq!(size(&reg, &mut ctx, LHM, lhm), Some(Value::Int(1)));
}

/// A `java/util/HashSet` receiver backed by a `LinkedHashMap` (see
/// `r13_hashcompat3_jdk_counts.rs`: the mock has no class hierarchy, so a
/// `LinkedHashSet` receiver would not find its backing).
fn new_linked_backed_set(reg: &NativeMethodRegistry, ctx: &mut MockCtx) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(HS).unwrap();
    let set = ctx.alloc_object(cid, 4);
    let backing = new_linked_hashmap(reg, ctx, false);
    ctx.set_field(set, 0, Value::Object(Some(backing)));
    set
}

fn new_plain_set(reg: &NativeMethodRegistry, ctx: &mut MockCtx) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(HS).unwrap();
    let set = ctx.alloc_object(cid, 4);
    call(reg, ctx, HS, "<init>", "()V", &[Value::Object(Some(set))]).unwrap();
    set
}

type SetMaker = fn(&NativeMethodRegistry, &mut MockCtx) -> ObjectRef;

/// `vm` must differ per call: two `MockCtx`s built on one thread are
/// otherwise the same VM (`vm_identity` 0) to the per-thread `(vm, ClassId)`
/// caches (`receiver_facts`, the well-known-class ids), and each context
/// numbers its classes from 1 -- so the second context's `java/util/HashMap`
/// backing inherited the first context's `LinkedHashMap` facts for the same
/// id and was served as a `LinkedHashMap`.
fn iterator_remove_hashes(set_of: SetMaker, vm: usize) -> (usize, Option<Value>) {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    ctx.set_vm_identity(vm);
    let set = set_of(&reg, &mut ctx);
    for _ in 0..3 {
        let k = user_key(&mut ctx);
        let added = call(&reg, &mut ctx, HS, "add", ADD, &[obj(set), obj(k)]).unwrap();
        assert_eq!(added, Some(Value::Int(1)));
    }
    let itr_desc = "()Ljava/util/Iterator;";
    let itr = match call(&reg, &mut ctx, HS, "iterator", itr_desc, &[obj(set)]).unwrap() {
        Some(Value::Object(Some(itr))) => itr,
        other => panic!("iterator() answered {other:?}"),
    };
    let carrier = class_name_of(&ctx, itr).expect("a named iterator carrier");
    call(&reg, &mut ctx, &carrier, "next", "()Ljava/lang/Object;", &[obj(itr)]).unwrap();
    ctx.clear_invoke_virtual_log();
    call(&reg, &mut ctx, &carrier, "remove", "()V", &[obj(itr)]).unwrap();
    let hashes = calls_of(&ctx, "hashCode");
    // Asked of the backing map through its OWN class's `size()`: in the mock a
    // `LinkedHashMap` keeps its size only in the overlay (`lhm_set` cannot
    // mirror it into a real field here), so `HashSet.size()` ->
    // `native_map_size` reads 0 for a LinkedHashMap-backed set.
    let backing = match ctx.get_field(set, 0) {
        Value::Object(Some(b)) => b,
        other => panic!("the set lost its backing map: {other:?}"),
    };
    let backing_class = class_name_of(&ctx, backing).expect("a named backing map");
    (hashes, size(&reg, &mut ctx, &backing_class, backing))
}

#[test]
fn r13_hashcompat4_iterator_remove_unlinks_the_node_without_hashing() {
    let (hashes, left) = iterator_remove_hashes(new_linked_backed_set, 0x4843_0401);
    assert_eq!(hashes, 0, "LinkedHashSet: removeNode(p.hash, ..) uses the stored hash");
    assert_eq!(left, Some(Value::Int(2)));
    let (hashes, left) = iterator_remove_hashes(new_plain_set, 0x4843_0402);
    assert_eq!(hashes, 0, "HashSet: removeNode(p.hash, ..) uses the stored hash");
    assert_eq!(left, Some(Value::Int(2)));
}

/// A live entry the way the natives mint one for a `LinkedHashMap`:
/// `AbstractMap$SimpleEntry` with the source map in slot 2.
fn live_entry(ctx: &mut MockCtx, key: ObjectRef, value: Value, source: ObjectRef) -> ObjectRef {
    let cid = ctx.ensure_class_initialized("java/util/AbstractMap$SimpleEntry").unwrap();
    let e = ctx.alloc_object(cid, 3);
    ctx.set_field(e, 0, Value::Object(Some(key)));
    ctx.set_field(e, 1, value);
    ctx.set_field(e, 2, Value::Object(Some(source)));
    e
}

fn set_value(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    entry: ObjectRef,
    v: Value,
) -> Option<Value> {
    call(
        reg,
        ctx,
        "java/util/AbstractMap$SimpleEntry",
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(entry)), v],
    )
    .unwrap()
}

#[test]
fn r13_hashcompat4_entry_set_value_writes_the_node_in_place() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, true);
    let a = user_key(&mut ctx);
    let b = user_key(&mut ctx);
    let va = value_obj(&mut ctx);
    let vb = value_obj(&mut ctx);
    put(&reg, &mut ctx, LHM, lhm, a, va);
    put(&reg, &mut ctx, LHM, lhm, b, vb);
    let entry = live_entry(&mut ctx, a, va, lhm);
    let fresh = value_obj(&mut ctx);
    ctx.set_invoke_virtual_results(Vec::new());
    ctx.clear_invoke_virtual_log();
    let old = set_value(&reg, &mut ctx, entry, fresh);
    assert_eq!(old, Some(va), "setValue answers the previous value");
    assert_eq!(calls_of(&ctx, "equals"), 0, "Node.setValue compares nothing");
    let pairs = for_each_pairs(&reg, &mut ctx, LHM, lhm);
    assert_eq!(
        pairs,
        vec![(Value::Object(Some(a)), fresh), (Value::Object(Some(b)), vb)],
        "the value is written and an access-ordered map does not reorder"
    );

    // A removed key is not put back.
    call(&reg, &mut ctx, LHM, "remove", REMOVE, &[obj(lhm), obj(a)]).unwrap();
    let later = value_obj(&mut ctx);
    set_value(&reg, &mut ctx, entry, later);
    assert_eq!(size(&reg, &mut ctx, LHM, lhm), Some(Value::Int(1)), "no resurrection");
}

#[test]
fn r13_hashcompat4_linked_reversed_does_not_hash() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let lhm = new_linked_hashmap(&reg, &mut ctx, false);
    let keys: Vec<ObjectRef> = (0..3).map(|_| user_key(&mut ctx)).collect();
    for k in &keys {
        let v = value_obj(&mut ctx);
        put(&reg, &mut ctx, LHM, lhm, *k, v);
    }
    ctx.set_invoke_virtual_results(Vec::new());
    ctx.clear_invoke_virtual_log();
    let rev_desc = "()Ljava/util/SequencedMap;";
    let rev = match call(&reg, &mut ctx, LHM, "reversed", rev_desc, &[obj(lhm)]).unwrap() {
        Some(Value::Object(Some(r))) => r,
        other => panic!("reversed() answered {other:?}"),
    };
    assert_eq!(calls_of(&ctx, "hashCode"), 0, "a reversed view hashes nothing");
    let order = keys_of(&for_each_pairs(&reg, &mut ctx, LHM, rev));
    let expected: Vec<Value> = keys.iter().rev().map(|k| Value::Object(Some(*k))).collect();
    assert_eq!(order, expected, "reverse encounter order");
}

#[test]
fn r13_hashcompat4_map_equals_asks_the_stored_value() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();
    let hm = new_hashmap(&reg, &mut ctx);
    let k = user_key(&mut ctx);
    let v = value_obj(&mut ctx);
    put(&reg, &mut ctx, HM, hm, k, v);
    // A foreign `Map`: its `size()` and `get(k)` are scripted.
    let other = user_object(&mut ctx, "java/util/Map");
    ctx.clear_invoke_virtual_log();
    ctx.set_invoke_virtual_results(vec![
        Ok(Some(Value::Int(1))),
        Ok(Some(v)),
        Ok(Some(Value::Int(1))),
    ]);
    let eq = call(
        &reg,
        &mut ctx,
        HM,
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(hm)), Value::Object(Some(other))],
    )
    .unwrap();
    assert_eq!(eq, Some(Value::Int(1)));
    let Value::Object(Some(v_ref)) = v else {
        unreachable!()
    };
    let asked: Vec<_> = ctx
        .invoke_virtual_log()
        .into_iter()
        .filter(|(_, m, _, _)| m == "equals")
        .collect();
    assert_eq!(asked.len(), 1, "value.equals(m.get(key)), even for the same object");
    assert_eq!(asked[0].0, v_ref.as_ptr() as usize, "the STORED value is the receiver");
}
