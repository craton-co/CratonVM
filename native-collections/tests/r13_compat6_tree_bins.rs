// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! Round 13 wave 11 (lane compat6): the `--compatible` native `HashMap` hands
//! a TREE bin (a `java.util.HashMap$TreeNode` head) to the JDK's own tree
//! code instead of walking its `next` list, and never links a plain node into
//! one. `CRATONVM_COMPAT_HASHMAP_TREE_BINS`; page
//! `r13w4-hashcompat-native-hashmap-never-treeifies-FIXED-20260929.md`, "Round 13
//! wave 11".
//!
//! This mock has no JDK bytecode: `invoke_special_bytecode_only` falls back to
//! `invoke`, which answers `Ok(None)` for `getTreeNode`, `removeNode` and
//! `putTreeVal`. So a lookup that the tree code serves answers "absent" here,
//! where the old chain walk found the key -- that difference is what these
//! tests pin: the natives asked the tree, not the chain. Treeifying itself
//! needs the real `HashMap.treeifyBin` bytecode and a real `table` field, so
//! it is covered by the Java probes (`R13IrhashMapShapes`,
//! `R13Compat6TreeBins`), and here only by the check that a map the natives
//! cannot treeify keeps working as a chain.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{build_registry, call, new_hashmap, MockCtx};
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::{ObjectRef, Value};

const HM: &str = "java/util/HashMap";
const TREE_NODE: &str = "java/util/HashMap$TreeNode";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const GET: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
const REMOVE: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
const CONTAINS_KEY: &str = "(Ljava/lang/Object;)Z";

/// `HashMap$TreeNode`'s width: `hash key value next` (`HashMap$Node`),
/// `before after` (`LinkedHashMap$Entry`), `parent left right prev red`.
const TREE_NODE_FIELDS: usize = 11;

fn fresh_ctx(vm: usize) -> MockCtx {
    let mut ctx = MockCtx::new();
    ctx.set_vm_identity(vm);
    ctx
}

fn obj(o: ObjectRef) -> Value {
    Value::Object(Some(o))
}

fn user_object(ctx: &mut MockCtx, class: &str) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class).unwrap();
    ctx.alloc_object(cid, 1)
}

fn calls_of(ctx: &MockCtx, method: &str) -> usize {
    ctx.invoke_virtual_log()
        .iter()
        .filter(|(_, m, _, _)| m == method)
        .count()
}

fn map_size(reg: &NativeMethodRegistry, ctx: &mut MockCtx, map: ObjectRef) -> Option<Value> {
    call(reg, ctx, HM, "size", "()I", &[obj(map)]).unwrap()
}

/// A `HashMap` holding `n` user keys that all hash to `hash` (one bucket),
/// each mapped to a fresh value.
fn colliding_map(
    reg: &NativeMethodRegistry,
    ctx: &mut MockCtx,
    hash: i32,
    n: usize,
) -> (ObjectRef, Vec<ObjectRef>, Vec<Value>) {
    let hm = new_hashmap(reg, ctx);
    let mut keys = Vec::new();
    let mut values = Vec::new();
    for _ in 0..n {
        let k = user_object(ctx, "test/R13Compat6Key");
        let v = obj(user_object(ctx, "test/R13Compat6Value"));
        // hashCode(k), then `equals` (false) against each key already in.
        let mut script = vec![Ok(Some(Value::Int(hash)))];
        script.extend((0..keys.len()).map(|_| Ok(Some(Value::Int(0)))));
        ctx.set_invoke_virtual_results(script);
        call(reg, ctx, HM, "put", PUT, &[obj(hm), obj(k), v]).unwrap();
        keys.push(k);
        values.push(v);
    }
    ctx.set_invoke_virtual_results(Vec::new());
    (hm, keys, values)
}

/// The mock keeps a `HashMap`'s table in slot 0; answer it and the index of
/// its one occupied bucket.
fn only_bin(ctx: &MockCtx, map: ObjectRef) -> (ObjectRef, usize) {
    let table = match ctx.get_field(map, 0) {
        Value::Object(Some(t)) => t,
        other => panic!("no table: {other:?}"),
    };
    let len = ctx.array_length(table);
    let occupied: Vec<usize> = (0..len)
        .filter(|i| !matches!(ctx.get_array_element(table, *i), Value::Object(None)))
        .collect();
    assert_eq!(occupied.len(), 1, "one colliding bucket");
    (table, occupied[0])
}

/// The nodes of a chain, head first.
fn chain(ctx: &MockCtx, head: Value) -> Vec<ObjectRef> {
    let mut out = Vec::new();
    let mut cur = head;
    while let Value::Object(Some(n)) = cur {
        out.push(n);
        assert!(out.len() < 64, "a cycle");
        cur = ctx.get_field(n, 3);
    }
    out
}

/// Replace the bin's plain nodes by `HashMap$TreeNode`s carrying the same
/// `hash`/`key`/`value`, in the same `next` order: the shape a JDK
/// `treeifyBin` leaves (tree links are not needed here -- the mock never runs
/// the tree code). Answers the tree nodes.
fn make_tree_bin(ctx: &mut MockCtx, map: ObjectRef) -> Vec<ObjectRef> {
    let (table, idx) = only_bin(ctx, map);
    let plain = chain(ctx, ctx.get_array_element(table, idx));
    let tcid = ctx.ensure_class_initialized(TREE_NODE).unwrap();
    let mut tree: Vec<ObjectRef> = Vec::new();
    for p in &plain {
        let t = ctx.alloc_object(tcid, TREE_NODE_FIELDS);
        for slot in 0..3 {
            let v = ctx.get_field(*p, slot);
            ctx.set_field(t, slot, v);
        }
        ctx.set_field(t, 3, Value::Object(None));
        if let Some(prev) = tree.last() {
            ctx.set_field(*prev, 3, obj(t));
        }
        tree.push(t);
    }
    ctx.set_array_element(table, idx, obj(tree[0]));
    tree
}

#[test]
fn r13_compat6_get_asks_the_tree_after_the_first_node() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x6c06_0001);
    let (hm, keys, _) = colliding_map(&reg, &mut ctx, 5, 2);
    make_tree_bin(&mut ctx, hm);
    ctx.clear_invoke_virtual_log();
    // hashCode(k1), then the first-node test `k1.equals(k0)` (false).
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(5))), Ok(Some(Value::Int(0)))]);
    let got = call(&reg, &mut ctx, HM, "get", GET, &[obj(hm), obj(keys[1])]).unwrap();
    // `getNode`: `first` did not match, so `((TreeNode) first).getTreeNode(
    // hash, key)` answers -- null in this bytecode-less mock. The chain walk
    // found k1 on the second node.
    assert_eq!(got, Some(Value::Object(None)));
    assert_eq!(calls_of(&ctx, "hashCode"), 1);
    assert_eq!(calls_of(&ctx, "equals"), 1, "only the first-node test ran natively");
}

#[test]
fn r13_compat6_contains_key_asks_the_tree_after_the_first_node() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x6c06_0002);
    let (hm, keys, _) = colliding_map(&reg, &mut ctx, 5, 2);
    make_tree_bin(&mut ctx, hm);
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(5))), Ok(Some(Value::Int(0)))]);
    let found = call(&reg, &mut ctx, HM, "containsKey", CONTAINS_KEY, &[obj(hm), obj(keys[1])])
        .unwrap();
    assert_eq!(found, Some(Value::Int(0)), "the tree answered, not the chain");
}

#[test]
fn r13_compat6_the_first_node_still_matches_natively() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x6c06_0003);
    let (hm, keys, values) = colliding_map(&reg, &mut ctx, 5, 2);
    make_tree_bin(&mut ctx, hm);
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(5)))]);
    // `first.key == key`: no `equals`, no tree.
    let got = call(&reg, &mut ctx, HM, "get", GET, &[obj(hm), obj(keys[0])]).unwrap();
    assert_eq!(got, Some(values[0]));
}

#[test]
fn r13_compat6_remove_from_a_tree_bin_is_the_jdk_remove_node() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x6c06_0004);
    let (hm, keys, _) = colliding_map(&reg, &mut ctx, 5, 2);
    let tree = make_tree_bin(&mut ctx, hm);
    ctx.clear_invoke_virtual_log();
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(5)))]);
    let removed = call(&reg, &mut ctx, HM, "remove", REMOVE, &[obj(hm), obj(keys[1])]).unwrap();
    // `removeNode(hash, key, null, false, true)` is the JDK's bytecode; the
    // mock's answers "nothing removed". The native chain unlink would have
    // taken the second node out of `next` and left the tree pointing at it.
    assert_eq!(removed, Some(Value::Object(None)));
    assert_eq!(calls_of(&ctx, "hashCode"), 1);
    assert_eq!(calls_of(&ctx, "equals"), 0, "the first-node test is removeNode's own");
    assert_eq!(map_size(&reg, &mut ctx, hm), Some(Value::Int(2)));
    let (table, idx) = only_bin(&ctx, hm);
    assert_eq!(chain(&ctx, ctx.get_array_element(table, idx)), tree);
}

#[test]
fn r13_compat6_put_never_appends_a_plain_node_to_a_tree_bin() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x6c06_0005);
    let (hm, _, _) = colliding_map(&reg, &mut ctx, 5, 2);
    let tree = make_tree_bin(&mut ctx, hm);
    let k2 = user_object(&mut ctx, "test/R13Compat6Key");
    let v2 = obj(user_object(&mut ctx, "test/R13Compat6Value"));
    // hashCode(k2), then the first-node test `k2.equals(k0)` (false); the
    // rest is `putTreeVal`.
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(5))), Ok(Some(Value::Int(0)))]);
    let old = call(&reg, &mut ctx, HM, "put", PUT, &[obj(hm), obj(k2), v2]).unwrap();
    assert_eq!(old, Some(Value::Object(None)));
    let (table, idx) = only_bin(&ctx, hm);
    assert_eq!(
        chain(&ctx, ctx.get_array_element(table, idx)),
        tree,
        "the native linked nothing itself: `putTreeVal` owns a tree bin's links"
    );
    // `putTreeVal` answered null (inserted), which `putVal` accounts.
    assert_eq!(map_size(&reg, &mut ctx, hm), Some(Value::Int(3)));
}

#[test]
fn r13_compat6_put_overwrites_the_first_node_of_a_tree_bin_in_place() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x6c06_0006);
    let (hm, keys, values) = colliding_map(&reg, &mut ctx, 5, 2);
    let tree = make_tree_bin(&mut ctx, hm);
    let v = obj(user_object(&mut ctx, "test/R13Compat6Value"));
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(5)))]);
    let old = call(&reg, &mut ctx, HM, "put", PUT, &[obj(hm), obj(keys[0]), v]).unwrap();
    assert_eq!(old, Some(values[0]));
    assert_eq!(ctx.get_field(tree[0], 2), v, "`e.value = value` on the TreeNode");
    assert_eq!(map_size(&reg, &mut ctx, hm), Some(Value::Int(2)));
}

#[test]
fn r13_compat6_a_map_that_cannot_treeify_keeps_a_long_chain() {
    // The mock has no real `table` field (and no `treeifyBin` bytecode), so
    // nine colliding keys stay one chain, every one of them reachable.
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x6c06_0007);
    let (hm, keys, values) = colliding_map(&reg, &mut ctx, 5, 9);
    assert_eq!(map_size(&reg, &mut ctx, hm), Some(Value::Int(9)));
    let (table, idx) = only_bin(&ctx, hm);
    assert_eq!(chain(&ctx, ctx.get_array_element(table, idx)).len(), 9);
    let mut script = vec![Ok(Some(Value::Int(5)))];
    script.extend((0..8).map(|_| Ok(Some(Value::Int(0)))));
    ctx.set_invoke_virtual_results(script);
    let got = call(&reg, &mut ctx, HM, "get", GET, &[obj(hm), obj(keys[8])]).unwrap();
    assert_eq!(got, Some(values[8]));
}

// ---------------------------------------------------------------------------
// `values().remove(o)` (`CRATONVM_COMPAT_VALUES_REMOVE_JDK`, residual page
// `r13w10-compat5-map-view-residuals-CLOSED-20260929.md` item 1).
// ---------------------------------------------------------------------------

#[test]
fn r13_compat6_values_remove_walks_the_live_map_and_hashes_nothing() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x6c06_0010);
    let hm = new_hashmap(&reg, &mut ctx);
    let keys: Vec<ObjectRef> = (0..3)
        .map(|_| user_object(&mut ctx, "test/R13Compat6Key"))
        .collect();
    let vals: Vec<Value> = (0..3)
        .map(|_| obj(user_object(&mut ctx, "test/R13Compat6Value")))
        .collect();
    // Buckets 1 and 2 before the view is taken, bucket 3 after.
    for i in 0..2 {
        ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(i as i32 + 1)))]);
        call(&reg, &mut ctx, HM, "put", PUT, &[obj(hm), obj(keys[i]), vals[i]]).unwrap();
    }
    let values = match call(&reg, &mut ctx, HM, "values", "()Ljava/util/Collection;", &[obj(hm)])
        .unwrap()
    {
        Some(Value::Object(Some(v))) => v,
        other => panic!("values() answered {other:?}"),
    };
    let carrier = common::class_name_of(&ctx, values).expect("a named values carrier");
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(3)))]);
    call(&reg, &mut ctx, HM, "put", PUT, &[obj(hm), obj(keys[2]), vals[2]]).unwrap();

    ctx.clear_invoke_virtual_log();
    // `vals[2].equals(v)` for the two values ahead of it (false); `vals[2]`
    // itself is found by identity.
    ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(0))), Ok(Some(Value::Int(0)))]);
    let removed = call(
        &reg,
        &mut ctx,
        &carrier,
        "remove",
        "(Ljava/lang/Object;)Z",
        &[obj(values), vals[2]],
    )
    .unwrap();
    assert_eq!(
        removed,
        Some(Value::Int(1)),
        "the view is live: a value put after it was taken is found"
    );
    assert_eq!(calls_of(&ctx, "hashCode"), 0, "`it.remove()` is removeNode(p.hash, ..)");
    assert_eq!(map_size(&reg, &mut ctx, hm), Some(Value::Int(2)));
    let got = {
        ctx.set_invoke_virtual_results(vec![Ok(Some(Value::Int(3)))]);
        call(&reg, &mut ctx, HM, "get", GET, &[obj(hm), obj(keys[2])]).unwrap()
    };
    assert_eq!(got, Some(Value::Object(None)), "the node of keys[2] went");
}
